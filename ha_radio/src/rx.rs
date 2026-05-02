#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use {
    board_support::{
        Board, I2sOutputPio, Pcm3060Board, Sx127xBoard, VolumeEncoderPio,
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
        peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, DMA_CH3, I2C0, PIO0, PIO1},
        pio::InterruptHandler as PioInterruptHandler,
        pwm::{Pwm, SetDutyCycle},
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
    PIO1_IRQ_0 => PioInterruptHandler<PIO1>;
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

    loop {
        let version = radio.read_version().await.unwrap();
        info!("radio: version=0x{:02x} (expect 0x12)", version);

        let gfsk_cfg = GfskRxConfig {
            frequency_hz: RADIO_FREQ_HZ,
            bitrate_bps: RADIO_BITRATE_BPS,
            sync_word: RADIO_SYNC_WORD,
            max_payload_len: OPUS_BUF_SIZE as u8,
        };
        radio.configure_gfsk_rx(&gfsk_cfg).await.unwrap();
        info!("radio: configured for rx");

        let mut pkt_buf = [0u8; 255];
        let mut timeouts = 0;
        'rx: loop {
            match radio.receive(&mut dio0, &mut pkt_buf).await {
                Ok(len) => {
                    timeouts = 0;
                    if len > OPUS_BUF_SIZE {
                        warn!("radio: pkt too large {}", len);
                        let mut opus = tx.send().await;
                        opus.len = 0;
                        opus.send_done();
                        continue;
                    }
                    // info!("rx: len={}", len);
                    // info!("rx: rssi={}dBm", radio.read_rssi_dbm().await.unwrap_or(0));
                    let mut opus = tx.send().await;
                    opus.data[..len].copy_from_slice(&pkt_buf[..len]);
                    opus.len = len;
                    opus.send_done();
                }
                Err(sx127x::Error::Timeout) => {
                    let rssi = radio.read_rssi_dbm().await.unwrap_or(0);

                    timeouts += 1;
                    info!("radio: rx timeout #{} rssi={}dBm", timeouts, rssi);
                    if timeouts > 10 {
                        break 'rx;
                    }
                }
                Err(sx127x::Error::CrcError) => {
                    warn!("radio: rx CRC error");
                    // send an empty packet to trigger PLC
                    let mut opus = tx.send().await;
                    opus.len = 0;
                    opus.send_done();
                }
                Err(e) => {
                    warn!("radio: rx err {}", defmt::Debug2Format(&e));
                }
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
            Err(e) => {
                error!(
                    "decode: failed ({})",
                    match e {
                        embedded_opus::Error::BadArg => "bad argument",
                        embedded_opus::Error::BufferTooSmall => "buffer too small",
                        embedded_opus::Error::InternalError => "internal error",
                        embedded_opus::Error::InvalidPacket => "invalid packet",
                        embedded_opus::Error::Unimplemented => "unimplemented (?)",
                        embedded_opus::Error::InvalidState => "invalid state",
                        embedded_opus::Error::AllocFail => "allocation failure",
                        _ => "!?",
                    }
                );
                if let Err(_) = decoder.plc(pcm) {
                    error!("decode: loss concealment failed");
                }
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
    mut rx: PackedAudioFrameReceiver,
    mut led_1: Pwm<'static>,
    mut led_2: Pwm<'static>,
) {
    i2s.start();
    info!("i2s out: started");
    loop {
        let buf = rx.receive().await;
        // Copy the first sample for LED metering before starting DMA
        let sample = buf[0];
        i2s.write(&*buf).await;
        buf.receive_done();

        let r_level = (((sample >> 16) as i16).saturating_abs() / (i16::MAX / 100)) as u8;
        let l_level = ((sample as i16).saturating_abs() / (i16::MAX / 100)) as u8;
        led_1.set_duty_cycle_percent(r_level).unwrap();
        led_2.set_duty_cycle_percent(l_level).unwrap();
    }
}

/// Codec control task -- currently just sets the volume
#[embassy_executor::task]
async fn codec_control_task(mut codec: Pcm3060Board, mut volume_knob: VolumeEncoderPio) {
    codec.reset().await.unwrap();
    codec.dac_init().await.unwrap();
    info!("codec configured");

    loop {
        volume_knob.poll().await;
        let vol = volume_knob.pos();

        // mute if volume is sufficiently low
        let vol = if vol == volume_knob.min() { 0 } else { vol };

        // NOTE: fine because of the type constraints
        if let Err(_) = codec.set_volume(vol as u8).await {
            error!("codec control: error setting volume");
        }
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    // Setup the board
    let mut board = Board::new(SystemConfig::default(), Irqs);

    // Turn off LEDs initially
    board.led_1.set_duty_cycle_percent(0).unwrap();
    board.led_2.set_duty_cycle_percent(0).unwrap();

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
        spawner.spawn(codec_control_task(board.codec, board.volume_knob).unwrap());
        spawner.spawn(
            radio_rx_task(radio_opus_tx, board.radio, board.radio_d0, board.radio_rst).unwrap(),
        );
        spawner.spawn(i2s_out_task(board.i2s_out, opus_i2s_rx, board.led_1, board.led_2).unwrap());
        spawner.spawn(amp_enable(board.out_det, board.amp_nshdn).unwrap());
    })
}
