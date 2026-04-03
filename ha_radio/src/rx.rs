#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

mod shared;

use {
    crate::shared::{
        CLOCK_HZ, FRAME_SAMPLES, OPUS_BUF_SIZE, OpusPacket, PackedAudioFrame, RADIO_BITRATE_BPS,
        RADIO_FREQ_HZ, RADIO_SYNC_WORD, SAMPLE_RATE, Spi0Bus, Sx127xConcrete,
    },
    core::{
        fmt::Write as _,
        ptr::addr_of_mut,
        sync::atomic::{AtomicI32, AtomicU32, Ordering},
    },
    defmt::*,
    defmt_rtt as _,
    embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice,
    embassy_executor::Executor,
    embassy_futures::join::join,
    embassy_rp::{
        bind_interrupts,
        clocks::ClockConfig,
        config::Config as SystemConfig,
        dma::InterruptHandler as DmaInterruptHandler,
        gpio::{Input, Level, Output, Pull},
        i2c::{self, I2c, InterruptHandler as I2cInterruptHandler},
        multicore::{Stack, spawn_core1},
        peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, I2C1, PIO0},
        pio::{InterruptHandler as PioInterruptHandler, Pio},
        pio_programs::i2s::{PioI2sOut, PioI2sOutProgram},
        spi::{Config as SpiConfig, Spi},
    },
    embassy_sync::{
        blocking_mutex::raw::CriticalSectionRawMutex,
        mutex::Mutex,
        zerocopy_channel::{Channel, Receiver, Sender},
    },
    embassy_time::Timer,
    embedded_graphics::{
        mono_font::{MonoTextStyleBuilder, ascii::FONT_5X8, ascii::FONT_6X10},
        pixelcolor::BinaryColor,
        prelude::*,
        text::Text,
    },
    embedded_opus::DECODER_STATE_SIZE_STEREO,
    panic_probe as _,
    ssd1306::{I2CDisplayInterface, Ssd1306Async, prelude::*, size::DisplaySize128x32},
    static_cell::StaticCell,
    sx127x::{GfskRxConfig, Sx127x},
};

static mut CORE1_STACK: Stack<262144> = Stack::new();
static EXECUTOR0: StaticCell<Executor> = StaticCell::new();
static EXECUTOR1: StaticCell<Executor> = StaticCell::new();

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => DmaInterruptHandler<DMA_CH0>, DmaInterruptHandler<DMA_CH1>, DmaInterruptHandler<DMA_CH2>;
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    I2C1_IRQ => I2cInterruptHandler<I2C1>;
});

// ----- Shared stats (updated by radio_rx_task, read by display_task)

static RSSI_DBM: AtomicI32 = AtomicI32::new(0);
static PKT_COUNT: AtomicU32 = AtomicU32::new(0);
static ERR_COUNT: AtomicU32 = AtomicU32::new(0);

// ----- TASKS
//
// Three-stage pipeline across two cores.
// Zerocopy channels with depth 2 connect adjacent stages.

type OpusPacketChannel = Channel<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketSender = Sender<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketReceiver = Receiver<'static, CriticalSectionRawMutex, OpusPacket>;

type PackedAudioFrameChannel = Channel<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameSender = Sender<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameReceiver = Receiver<'static, CriticalSectionRawMutex, PackedAudioFrame>;

/// Radio RX — Core 0.  Receives GFSK packets and forwards to the decoder.
/// DIO0 is mapped to PayloadReady in FSK packet mode.
#[embassy_executor::task]
async fn radio_rx_task(
    mut tx: OpusPacketSender,
    mut radio: Sx127xConcrete,
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
        };
        radio.configure_gfsk_rx(&gfsk_cfg).await.unwrap();
        info!("radio: configured for rx");

        let mut pkt_buf = [0u8; 255];
        let mut timeouts = 0;
        'rx: loop {
            match radio.receive(&mut dio0, &mut pkt_buf).await {
                Ok(len) => {
                    timeouts = 0;

                    let rssi = radio.read_rssi_dbm().await.unwrap_or(0);
                    RSSI_DBM.store(rssi, Ordering::Relaxed);
                    if len > OPUS_BUF_SIZE {
                        ERR_COUNT.fetch_add(1, Ordering::Relaxed);
                        warn!("radio: pkt too large {}", len);
                        continue;
                    }
                    // info!("rx: len={}", len);
                    // info!("rx: rssi={}dBm", rssi);
                    PKT_COUNT.fetch_add(1, Ordering::Relaxed);
                    let opus = tx.send().await;
                    opus.data[..len].copy_from_slice(&pkt_buf[..len]);
                    opus.len = len;
                    tx.send_done();
                }
                Err(sx127x::Error::Timeout) => {
                    let rssi = radio.read_rssi_dbm().await.unwrap_or(0);
                    RSSI_DBM.store(rssi, Ordering::Relaxed);

                    timeouts += 1;
                    info!("radio: rx timeout #{} rssi={}dBm", timeouts, rssi);
                    if timeouts > 10 {
                        break 'rx;
                    }
                }
                Err(sx127x::Error::CrcError) => {
                    warn!("radio: rx CRC error");
                    // send an empty packet to trigger PLC
                    let opus = tx.send().await;
                    opus.len = 0;
                    tx.send_done();
                }
                Err(e) => {
                    ERR_COUNT.fetch_add(1, Ordering::Relaxed);
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
        let (opus, pcm) = join(rx.receive(), tx.send()).await;
        let pcm: &mut [i16] = bytemuck::cast_slice_mut(pcm.as_mut_slice());
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
        rx.receive_done();
        tx.send_done();
    }
}

/// I2S TX output — Core 0.  Plays decoded PCM on the PCM5102A DAC.
#[embassy_executor::task]
async fn i2s_out_task(mut i2s: PioI2sOut<'static, PIO0, 0>, mut rx: PackedAudioFrameReceiver) {
    i2s.start();
    info!("i2s out: started");
    loop {
        let buf = rx.receive().await;
        i2s.write(buf).await;
        rx.receive_done();
    }
}

/// OLED display — Core 0.  Updates every 500ms with radio stats.
#[embassy_executor::task]
async fn display_task(i2c: I2c<'static, I2C1, i2c::Async>) {
    let interface = I2CDisplayInterface::new(i2c);
    let mut display = Ssd1306Async::new(interface, DisplaySize128x32, DisplayRotation::Rotate0)
        .into_buffered_graphics_mode();
    display.init().await.unwrap();

    // Yellow bar (rows 0-7): 5x8 fits cleanly in 8px
    let yellow = MonoTextStyleBuilder::new()
        .font(&FONT_5X8)
        .text_color(BinaryColor::On)
        .build();
    // Blue area (rows 8-31): 6x10 gives two rows with spacing
    let blue = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::On)
        .build();

    let mut buf = [0u8; 64];
    loop {
        display.clear_buffer();

        let rssi = RSSI_DBM.load(Ordering::Relaxed);
        let pkts = PKT_COUNT.load(Ordering::Relaxed);
        let errs = ERR_COUNT.load(Ordering::Relaxed);

        // Yellow bar: "HA//26" left, RSSI right
        let mut w = FmtBuf::new(&mut buf);
        core::write!(w, "HA//26    RSSI:{} dBm", rssi).ok();
        Text::new(w.as_str(), Point::new(0, 7), yellow)
            .draw(&mut display)
            .ok();

        // Blue row 1: packet count
        w.reset();
        core::write!(w, "Pkts: {}", pkts).ok();
        Text::new(w.as_str(), Point::new(0, 20), blue)
            .draw(&mut display)
            .ok();

        // Blue row 2: error count
        w.reset();
        core::write!(w, "Errs: {}", errs).ok();
        Text::new(w.as_str(), Point::new(0, 31), blue)
            .draw(&mut display)
            .ok();

        display.flush().await.ok();
        Timer::after_secs(2).await;
    }
}

/// Tiny no_alloc format buffer.
struct FmtBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> FmtBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn reset(&mut self) {
        self.pos = 0;
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.pos]).unwrap_or("")
    }
}

impl core::fmt::Write for FmtBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let n = bytes.len().min(remaining);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let mut config = SystemConfig::default();
    config.clocks = ClockConfig::system_freq(CLOCK_HZ).unwrap();
    let p = embassy_rp::init(config);
    info!("rx: booting at {} Hz", CLOCK_HZ);

    // Grab all the pins
    // SX1276 (RFM95W)
    let sck = p.PIN_2;
    let mosi = p.PIN_3;
    let miso = p.PIN_4;
    let cs = p.PIN_5;
    let dio0 = Input::new(p.PIN_6, Pull::Down); // DIO0 / G0 → PayloadReady
    let rst = Output::new(p.PIN_7, Level::High); // RST — hardware reset
    // SSD1306 OLED (I2C1)
    let sda = p.PIN_14;
    let scl = p.PIN_15;
    // PCM5102A
    let data_out = p.PIN_22;
    let bck_out = p.PIN_27;
    let lrck_out = p.PIN_28;

    // ----- Configure I2C1 for the OLED display
    let mut i2c_config = i2c::Config::default();
    i2c_config.frequency = 400_000;
    let i2c = I2c::new_async(p.I2C1, scl, sda, Irqs, i2c_config);

    // ----- Configure SPI and the Radio
    let mut config = SpiConfig::default();
    config.frequency = 10_000_000;
    let spi = Spi::new(p.SPI0, sck, mosi, miso, p.DMA_CH0, p.DMA_CH1, Irqs, config);
    static SPI_BUS: StaticCell<Spi0Bus> = StaticCell::new();
    let spi_bus = SPI_BUS.init(Mutex::new(spi));
    let cs = Output::new(cs, Level::High);
    let spi_dev = SpiDevice::new(spi_bus, cs);
    let radio = Sx127x::new(spi_dev);

    // ----- PIO I2S TX output → PCM5102A
    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);
    let program = PioI2sOutProgram::new(&mut common);
    let i2s = PioI2sOut::new(
        &mut common,
        sm0,
        p.DMA_CH2,
        Irqs,
        data_out,
        bck_out,
        lrck_out,
        SAMPLE_RATE,
        16,
        &program,
    );

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
        p.CORE1,
        unsafe { &mut *addr_of_mut!(CORE1_STACK) },
        move || {
            let executor1 = EXECUTOR1.init(Executor::new());
            executor1.run(|spawner| {
                spawner.spawn(opus_decode_task(radio_opus_rx, opus_i2s_tx).unwrap());
            });
        },
    );

    // ----- Core 0: radio RX + I2S TX output + OLED display (all DMA lives here)
    let executor0 = EXECUTOR0.init(Executor::new());
    executor0.run(|spawner| {
        spawner.spawn(radio_rx_task(radio_opus_tx, radio, dio0, rst).unwrap());
        spawner.spawn(i2s_out_task(i2s, opus_i2s_rx).unwrap());
        spawner.spawn(display_task(i2c).unwrap());
    })
}
