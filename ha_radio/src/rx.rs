#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use {
    board_support::{
        Board, I2sOutputPio, Pcm3060Board, Sx127xBoard,
        consts::{
            FRAME_SAMPLES, OPUS_BUF_SIZE, OpusPacket, PackedAudioFrame, RADIO_BITRATE_BPS,
            RADIO_FREQ_HZ, RADIO_SYNC_WORD, SAMPLE_RATE,
        },
    },
    core::ptr::addr_of_mut,
    defmt::*,
    defmt_rtt as _,
    embassy_executor::Executor,
    embassy_futures::join::join,
    embassy_rp::{
        bind_interrupts,
        config::Config as SystemConfig,
        dma::InterruptHandler as DmaInterruptHandler,
        gpio::{Input, Output},
        i2c::InterruptHandler as I2cInterruptHandler,
        multicore::{Stack, spawn_core1},
        peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, DMA_CH3, I2C0, PIO0},
        pio::InterruptHandler as PioInterruptHandler,
    },
    embassy_sync::{
        blocking_mutex::raw::CriticalSectionRawMutex,
        zerocopy_channel::{Channel, Receiver, Sender},
    },
    embassy_time::Timer,
    embedded_opus::DECODER_STATE_SIZE_STEREO,
    panic_probe as _,
    static_cell::StaticCell,
    sx127x::GfskRxConfig,
};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => DmaInterruptHandler<DMA_CH0>, DmaInterruptHandler<DMA_CH1>, DmaInterruptHandler<DMA_CH2>, DmaInterruptHandler<DMA_CH3>;
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    I2C0_IRQ => I2cInterruptHandler<I2C0>;
});

static mut CORE1_STACK: Stack<262144> = Stack::new();
static EXECUTOR0: StaticCell<Executor> = StaticCell::new();
static EXECUTOR1: StaticCell<Executor> = StaticCell::new();

type PackedAudioFrameChannel = Channel<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameSender = Sender<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameReceiver = Receiver<'static, CriticalSectionRawMutex, PackedAudioFrame>;

type OpusPacketChannel = Channel<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketSender = Sender<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketReceiver = Receiver<'static, CriticalSectionRawMutex, OpusPacket>;

#[embassy_executor::task]
async fn amp_enable(out_det: Input<'static>, mut amp_nshdn: Output<'static>) {
    info!("monitoring audio out jack");
    let mut enabled = false;
    loop {
        if out_det.is_low() && !enabled {
            info!("Enabling amplifier");
            enabled = true;
            amp_nshdn.set_high();
        } else if out_det.is_high() && enabled {
            info!("Disabling amplifier");
            enabled = false;
            amp_nshdn.set_low();
        }
        Timer::after_millis(100).await;
    }
}

/// Radio RX — Core 0.  Receives GFSK packets and forwards to the decoder.
/// DIO0 is mapped to PayloadReady in FSK packet mode.
#[embassy_executor::task]
async fn radio_rx_task(
    mut tx: OpusPacketSender,
    mut radio: Sx127xBoard,
    mut dio0: Input<'static>,
    mut rst: Output<'static>,
) {
    // Hardware reset: low ≥100µs, then wait ≥5ms for oscillator startup.
    rst.set_low();
    Timer::after_millis(1).await;
    rst.set_high();
    Timer::after_millis(10).await;

    let version = radio.read_version().await.unwrap();
    info!("radio: version=0x{:02x} (expect 0x12)", version);

    let gfsk_cfg = GfskRxConfig {
        frequency_hz: RADIO_FREQ_HZ,
        bitrate_bps: RADIO_BITRATE_BPS,
        sync_word: RADIO_SYNC_WORD,
    };
    radio.configure_gfsk_rx(&gfsk_cfg).await.unwrap();
    info!("radio: configured for rx");

    let mut pkt_buf = [0u8; 255];
    // Count consecutive receive() failures so we can force a full
    // re-configure after a sustained dropout.  Resets to 0 on any
    // successful packet.
    let mut consecutive_failures: u32 = 0;
    loop {
        match radio.receive(&mut dio0, &mut pkt_buf).await {
            Ok(len) => {
                consecutive_failures = 0;
                if len > OPUS_BUF_SIZE {
                    warn!("radio: pkt too large {}", len);
                    continue;
                }
                info!("rx: len={}", len);
                let mut opus = tx.send().await;
                opus.data[..len].copy_from_slice(&pkt_buf[..len]);
                opus.len = len;
                opus.send_done();
            }
            Err(sx127x::Error::Timeout) => {
                consecutive_failures += 1;
                let rssi = radio.read_rssi_dbm().await.unwrap_or(0);
                info!(
                    "radio: rx timeout rssi={}dBm (fail#{})",
                    rssi, consecutive_failures
                );
                // After 5 consecutive 1-second timeouts (~5 s of silence),
                // force a full reconfiguration.  The minimal recovery inside
                // receive() (FifoOverrun + Mode::Rx write) may not be enough
                // if the radio's internal FSM is confused — configure_gfsk_rx
                // runs the proper Sleep → Standby → Rx sequence with PLL-lock
                // wait, which is the only reliable way to recover.
                if consecutive_failures % 5 == 0 {
                    warn!(
                        "radio: reconfiguring after {} timeouts",
                        consecutive_failures
                    );
                    if let Err(e) = radio.configure_gfsk_rx(&gfsk_cfg).await {
                        warn!("radio: reconfigure failed: {}", defmt::Debug2Format(&e));
                    }
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!("radio: rx err {}", defmt::Debug2Format(&e));
            }
        }
    }
}

/// Opus decoder — Core 1.  CPU-bound, runs on its own executor so it
/// never starves the I2S output DMA on Core 0.
#[embassy_executor::task]
async fn opus_decode_task(mut rx: OpusPacketReceiver, mut tx: PackedAudioFrameSender) {
    let mut state_buf = [0u8; DECODER_STATE_SIZE_STEREO];
    let mut decoder = embedded_opus::Decoder::new(&mut state_buf, SAMPLE_RATE as i32, 2).unwrap();
    info!("decode: starting");
    loop {
        let (opus, mut pcm_slot) = join(rx.receive(), tx.send()).await;
        let pcm: &mut [i16] = bytemuck::cast_slice_mut(&mut (*pcm_slot));
        match decoder.decode(&opus.data[0..opus.len], pcm, false) {
            Ok(_len) => {}
            Err(_e) => {
                error!("decode: failed");
                decoder.plc(pcm).ok();
            }
        }
        opus.receive_done();
        pcm_slot.send_done();
    }
}

/// I2S TX output — Core 0.  Plays decoded PCM on the PCM5102A DAC.
#[embassy_executor::task]
async fn i2s_out_task(
    mut i2s: I2sOutputPio,
    mut codec: Pcm3060Board,
    mut rx: PackedAudioFrameReceiver,
) {
    codec.reset().await.unwrap();
    codec.dac_init().await.unwrap();
    info!("codec configured");

    // Double-buffer: pre-fetch next frame during DMA playback so the
    // gap between DMA transfers is just a function call, not a channel wait.
    let mut buf_a = [0u32; FRAME_SAMPLES / 2];
    let mut buf_b = [0u32; FRAME_SAMPLES / 2];

    // Prime: fill first buffer before starting playback (avoids startup clicks)
    let slot = rx.receive().await;
    buf_a.copy_from_slice(&*slot);
    slot.receive_done();

    i2s.start();
    info!("i2s out: started");

    loop {
        // Play buf_a, pre-fetch into buf_b
        let transfer = i2s.write(&buf_a);
        let slot = rx.receive().await;
        buf_b.copy_from_slice(&*slot);
        slot.receive_done();
        transfer.await;

        // Play buf_b, pre-fetch into buf_a
        let transfer = i2s.write(&buf_b);
        let slot = rx.receive().await;
        buf_a.copy_from_slice(&*slot);
        slot.receive_done();
        transfer.await;
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    // Setup the board
    let board = Board::new(SystemConfig::default(), Irqs);

    // ----- Set up the two zerocopy channels

    // Radio -> Opus decode (depth 3: absorbs radio RX jitter / SPI stalls)
    static RADIO_OPUS_BUF: StaticCell<[OpusPacket; 3]> = StaticCell::new();
    let radio_opus_buf = RADIO_OPUS_BUF.init(
        [OpusPacket {
            data: [0; OPUS_BUF_SIZE],
            len: 0,
        }; 3],
    );
    static RADIO_OPUS_CHAN: StaticCell<OpusPacketChannel> = StaticCell::new();
    let radio_opus_chan = RADIO_OPUS_CHAN.init(Channel::new(radio_opus_buf));
    let (radio_opus_tx, radio_opus_rx) = radio_opus_chan.split();

    // Opus decode -> I2S output (depth 3: absorbs decode time variance)
    static OPUS_I2S_BUF: StaticCell<[PackedAudioFrame; 3]> = StaticCell::new();
    let opus_i2s_buf = OPUS_I2S_BUF.init([[0; FRAME_SAMPLES / 2]; 3]);
    static OPUS_I2S_CHAN: StaticCell<PackedAudioFrameChannel> = StaticCell::new();
    let opus_i2s_chan = OPUS_I2S_CHAN.init(Channel::new(opus_i2s_buf));
    let (opus_i2s_tx, opus_i2s_rx) = opus_i2s_chan.split();

    // ----- Core 1: Opus decoding (pure CPU, no DMA needed)
    spawn_core1(
        board.core_1,
        unsafe { &mut *addr_of_mut!(CORE1_STACK) },
        move || {
            let executor1 = EXECUTOR1.init(Executor::new());
            executor1.run(|spawner| {
                spawner.spawn(opus_decode_task(radio_opus_rx, opus_i2s_tx).unwrap());
            });
        },
    );

    // ----- Core 0: radio RX + I2S TX output (all DMA lives here)
    let executor0 = EXECUTOR0.init(Executor::new());
    executor0.run(|spawner| {
        spawner.spawn(
            radio_rx_task(radio_opus_tx, board.radio, board.radio_d0, board.radio_rst).unwrap(),
        );
        spawner.spawn(i2s_out_task(board.i2s_out, board.codec, opus_i2s_rx).unwrap());
        spawner.spawn(amp_enable(board.out_det, board.amp_nshdn).unwrap());
    })
}
