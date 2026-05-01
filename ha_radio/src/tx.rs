#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use {
    board_support::{
        Board, I2sInputPio, Pcm3060Board, Sx127xBoard,
        consts::{
            FRAME_SAMPLES, OPUS_BUF_SIZE, OpusPacket, PackedAudioFrame, RADIO_BITRATE_BPS,
            RADIO_FDEV_HZ, RADIO_FREQ_HZ, RADIO_SYNC_WORD, SAMPLE_RATE,
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
    },
    embassy_sync::{
        blocking_mutex::raw::CriticalSectionRawMutex,
        zerocopy_channel::{Channel, Receiver, Sender},
    },
    embassy_time::Timer,
    embedded_opus::{Application, ENCODER_STATE_SIZE_STEREO, Encoder},
    panic_probe as _,
    static_cell::StaticCell,
    sx127x::{GfskConfig, ModulationShaping},
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
async fn i2s_in_task(
    mut i2s: I2sInputPio,
    mut codec: Pcm3060Board,
    mut tx: PackedAudioFrameSender,
) {
    codec.reset().await.unwrap();
    codec.adc_init().await.unwrap();
    info!("codec configured");

    // Start the I2S input
    i2s.start();
    info!("i2s in: started");
    loop {
        let t = embassy_time::Instant::now();
        let mut buf = tx.send().await;
        let wait_us = t.elapsed().as_micros();
        if wait_us > 100 {
            warn!("i2s in: stalled {}us waiting for free buffer", wait_us);
        }
        i2s.read(&mut *buf).await;
        buf.send_done();
    }
}

#[embassy_executor::task]
async fn opus_encode_task(mut rx: PackedAudioFrameReceiver, mut tx: OpusPacketSender) {
    let mut state_buf = [0u8; ENCODER_STATE_SIZE_STEREO];
    let mut encoder =
        Encoder::new(&mut state_buf, SAMPLE_RATE as i32, 2, Application::Audio).unwrap();
    encoder.set_bitrate(64_000).unwrap(); // 64kbps
    encoder.set_complexity(5).unwrap(); // Middle of the road, benched ok
    info!("encode: starting");
    loop {
        // Acquire both slots concurrently to avoid holding the PCM buffer
        // while waiting for a free packet slot
        let (pcm, mut opus) = join(rx.receive(), tx.send()).await;
        // bytemuck reinterprets [u32] as [i16] in-place (zero-copy).
        // Little-endian: each u32 (left<<16)|right becomes [right, left] as i16.
        // Sample VALUES are correct native-endian signed integers.
        // Channel order is [R, L, R, L, ...] — swapped, but Opus takes
        // native-endian i16 (not big-endian), so values are correct.
        // The decode side uses the same layout, making the round-trip consistent.
        let pcm_raw: &[i16] = bytemuck::cast_slice(pcm.as_slice());
        // Encode into the packet buffer (sized to the radio payload limit).
        match encoder.encode(pcm_raw, &mut opus.data) {
            Ok(len) => opus.len = len,
            Err(_e) => {
                error!("encode: failed");
                opus.len = 0;
            }
        }
        // Release PCM buffer first so I2S DMA can reclaim it
        pcm.receive_done();
        opus.send_done();
    }
}

/// Radio TX — Core 0.  SPI DMA + 1ms timer polls for FIFO refill, DIO0
/// interrupt for PacketSent.
#[embassy_executor::task]
async fn radio_transmit_task(
    mut rx: OpusPacketReceiver,
    mut radio: Sx127xBoard,
    mut dio0: Input<'static>,
    mut rst: Output<'static>,
) {
    // Hardware reset: low ≥100µs, then wait ≥5ms for oscillator startup.
    rst.set_low();
    Timer::after_millis(1).await;
    rst.set_high();
    Timer::after_millis(10).await;

    let config = GfskConfig {
        frequency_hz: RADIO_FREQ_HZ,
        bitrate_bps: RADIO_BITRATE_BPS,
        fdev_hz: RADIO_FDEV_HZ,
        tx_power_dbm: 20,
        modulation_shaping: ModulationShaping::GaussianBt05,
        sync_word: RADIO_SYNC_WORD,
    };
    radio.configure_gfsk_tx(&config).await.unwrap();
    info!("radio: configured, starting tx loop");

    loop {
        let packet = rx.receive().await;
        if packet.len != 0 {
            let data = &packet.data[..packet.len];
            match radio.transmit(&mut dio0, &data).await {
                Ok(()) => {}
                Err(e) => warn!("radio: tx err {}", e),
            }
        }
        packet.receive_done();
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    // Setup the board
    let board = Board::new(SystemConfig::default(), Irqs);

    // ----- Set up the two zero-copy channels

    // I2S -> OPUS
    static I2S_OPUS_BUF: StaticCell<[PackedAudioFrame; 2]> = StaticCell::new();
    let i2s_opus_buf = I2S_OPUS_BUF.init([[0; FRAME_SAMPLES / 2]; 2]);
    static I2S_OPUS_CHAN: StaticCell<PackedAudioFrameChannel> = StaticCell::new();
    let i2s_opus_chan = I2S_OPUS_CHAN.init(Channel::new(i2s_opus_buf));
    let (i2s_opus_tx, i2s_opus_rx) = i2s_opus_chan.split();

    // OPUS -> Radio
    static ENCODE_RADIO_BUF: StaticCell<[OpusPacket; 2]> = StaticCell::new();
    let encode_radio_buf = ENCODE_RADIO_BUF.init(
        [OpusPacket {
            data: [0; OPUS_BUF_SIZE],
            len: 0,
        }; 2],
    );
    static ENCODE_RADIO_CHAN: StaticCell<OpusPacketChannel> = StaticCell::new();
    let encode_radio_chan = ENCODE_RADIO_CHAN.init(Channel::new(encode_radio_buf));
    let (encode_radio_tx, encode_radio_rx) = encode_radio_chan.split();

    // ----- Core 1: Opus encoding (pure CPU, no DMA needed)
    spawn_core1(
        board.core_1,
        unsafe { &mut *addr_of_mut!(CORE1_STACK) },
        move || {
            let executor1 = EXECUTOR1.init(Executor::new());
            executor1.run(|spawner| {
                spawner.spawn(opus_encode_task(i2s_opus_rx, encode_radio_tx).unwrap());
            });
        },
    );

    // ----- Core 0: I2S capture + radio TX (all DMA lives here)
    let executor0 = EXECUTOR0.init(Executor::new());
    executor0.run(|spawner| {
        spawner.spawn(i2s_in_task(board.i2s_in, board.codec, i2s_opus_tx).unwrap());
        spawner.spawn(
            radio_transmit_task(
                encode_radio_rx,
                board.radio,
                board.radio_d0,
                board.radio_rst,
            )
            .unwrap(),
        );
    })
}
