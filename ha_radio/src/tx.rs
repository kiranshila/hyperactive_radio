#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

mod shared;

use {
    crate::shared::{
        CLOCK_HZ, FRAME_SAMPLES, OPUS_BUF_SIZE, OpusPacket, PackedAudioFrame, RADIO_BITRATE_BPS,
        RADIO_FDEV_HZ, RADIO_FREQ_HZ, RADIO_SYNC_WORD, SAMPLE_RATE, Spi0Bus, Sx127xConcrete,
    },
    core::ptr::addr_of_mut,
    defmt::*,
    defmt_rtt as _,
    embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice,
    embassy_executor::Executor,
    embassy_futures::join::join,
    embassy_rp::{
        Peri, bind_interrupts,
        clocks::ClockConfig,
        config::Config as SystemConfig,
        dma::{
            self, Channel as DmaChannel, ChannelInstance, InterruptHandler as DmaInterruptHandler,
            Transfer,
        },
        gpio::{Input, Level, Output, Pull},
        interrupt::typelevel::Binding,
        multicore::{Stack, spawn_core1},
        peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, PIO0},
        pio::{
            Common, Config as PioConfig, Direction, FifoJoin, Instance as PioInstance,
            InterruptHandler as PioInterruptHandler, LoadedProgram, Pio, PioPin, ShiftConfig,
            ShiftDirection, StateMachine, program::pio_asm,
        },
        pwm::{Config as PwmConfig, Pwm, SetDutyCycle},
        spi::{Config as SpiConfig, Spi},
    },
    embassy_sync::{
        blocking_mutex::raw::CriticalSectionRawMutex,
        mutex::Mutex,
        zerocopy_channel::{Channel, Receiver, Sender},
    },
    embassy_time::Timer,
    embedded_opus::{Application, ENCODER_STATE_SIZE_STEREO, Encoder},
    fixed::{FixedU16, types::extra::U4},
    panic_probe as _,
    static_cell::StaticCell,
    sx127x::{GfskConfig, ModulationShaping, Sx127x},
};

static mut CORE1_STACK: Stack<262144> = Stack::new();
static EXECUTOR0: StaticCell<Executor> = StaticCell::new();
static EXECUTOR1: StaticCell<Executor> = StaticCell::new();

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => DmaInterruptHandler<DMA_CH0>, DmaInterruptHandler<DMA_CH1>, DmaInterruptHandler<DMA_CH2>;
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
});

// ----- I2S Input Program

struct PioI2sInputProgram<'d, PIO: PioInstance> {
    prg: LoadedProgram<'d, PIO>,
}

impl<'d, PIO: PioInstance> PioI2sInputProgram<'d, PIO> {
    /// Load the program into the pio hardware
    pub fn new(common: &mut Common<'d, PIO>) -> Self {
        // Pin mapping (set_in_pins base = DATA):
        //   pin 0 = DATA (GPIO 26)
        //   pin 1 = BCK  (GPIO 27)
        //   pin 2 = LRCK (GPIO 28)
        let prg = pio_asm! {
            "start:",
            "   wait 1 pin 2",     // Sync word by watching a state transition on LRCK
            "sync_l:"
            "   wait 0 pin 2",     // Wait for transition to L on LRCK
            "   set y, 1",         // Set state to L (1)
            "   wait 0 pin 1",     // Ensure we're past the BCK falling edge
            "   wait 1 pin 1",     // Skip first rising edge for bit clock
            "read_word:"
            "   set x, 15",        // Reset the bit counter
            "read_bit:",
            "   wait 0 pin 1",     // Wait for BCK LOW
            "   wait 1 pin 1",     // Wait for BCK HIGH (sample DOUT on rising edge)
            "   in pins, 1",       // Shift one data bit into input shift register
            "   jmp x-- read_bit", // Loop to read 16 bits
            "   jmp !y sync_l",    // If y is 0 (R) jump to sync on L
            "   wait 1 pin 2",     // Wait for the transition to R on LRCK
            "   set y, 0",         // Set state to R (0)
            "   wait 0 pin 1",     // Ensure we're past the BCK falling edge
            "   wait 1 pin 1",     // Skip first rising edge for next channel
            "   jmp read_word",    // Loop to read another word
        };
        let prg = common.load_program(&prg.program);
        Self { prg }
    }
}

/// The PIO-backed I2S input peripheral
struct I2sInput<'d, P: PioInstance, const S: usize> {
    dma: DmaChannel<'d>,
    sm: StateMachine<'d, P, S>,
}

impl<'d, P: PioInstance, const S: usize> I2sInput<'d, P, S> {
    /// Setup and configure the I2sInput
    fn new<D, IRQ>(
        common: &mut Common<'d, P>,
        mut sm: StateMachine<'d, P, S>,
        dma: Peri<'d, D>,
        irq: IRQ,
        data: Peri<'d, impl PioPin>,
        bck: Peri<'d, impl PioPin>,
        lrck: Peri<'d, impl PioPin>,
        program: &PioI2sInputProgram<'d, P>,
    ) -> Self
    where
        D: ChannelInstance,
        IRQ: Binding<D::Interrupt, dma::InterruptHandler<D>> + 'd,
    {
        // Make all the PIO pins
        let data = common.make_pio_pin(data);
        let bck = common.make_pio_pin(bck);
        let lrck = common.make_pio_pin(lrck);

        let mut cfg = PioConfig::default();
        cfg.use_program(&program.prg, &[]);
        cfg.set_in_pins(&[&data, &bck, &lrck]);
        cfg.shift_in = ShiftConfig {
            threshold: 32,
            direction: ShiftDirection::Left,
            auto_fill: true,
        };
        // Double the RX FIFO depth for DMA headroom
        cfg.fifo_join = FifoJoin::RxOnly;

        // Attach configuration to state machine
        sm.set_config(&cfg);
        sm.set_pin_dirs(Direction::In, &[&data, &lrck, &bck]);
        Self {
            dma: DmaChannel::new(dma, irq),
            sm,
        }
    }

    /// Start the state machine
    fn start(&mut self) {
        self.sm.set_enable(true);
    }

    /// Return an in-progress dma transfer future.
    /// Awaiting this will guaruntee a complete transfer.
    fn read<'b>(&'b mut self, buff: &'b mut [u32]) -> Transfer<'b> {
        self.sm.rx().dma_pull(&mut self.dma, buff, false)
    }
}

// ----- TASKS
//
// Three-stage pipeline across two cores.
// Zerocopy channels with depth 2 connect adjacent stages.

type PackedAudioFrameChannel = Channel<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameSender = Sender<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameReceiver = Receiver<'static, CriticalSectionRawMutex, PackedAudioFrame>;

type OpusPacketChannel = Channel<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketSender = Sender<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketReceiver = Receiver<'static, CriticalSectionRawMutex, OpusPacket>;

/// I2S capture — Core 0.  DMA-driven, yields for the full 20ms frame period.
#[embassy_executor::task]
async fn i2s_in_task(mut i2s: I2sInput<'static, PIO0, 0>, mut tx: PackedAudioFrameSender) {
    // Wait for 8960/fs for the PCM1808 to output valid data
    Timer::after_millis(200).await;
    i2s.start();
    info!("i2s in: started");
    loop {
        let t = embassy_time::Instant::now();
        let buf = tx.send().await;
        let wait_us = t.elapsed().as_micros();
        if wait_us > 100 {
            warn!("i2s in: stalled {}us waiting for free buffer", wait_us);
        }
        i2s.read(buf).await;
        tx.send_done();
    }
}

/// Opus encoder — Core 1.  CPU-bound (~18ms), runs on its own executor so it
/// never starves the radio FIFO refill loop on Core 0.
#[embassy_executor::task]
async fn opus_encode_task(mut rx: PackedAudioFrameReceiver, mut tx: OpusPacketSender) {
    let mut state_buf = [0u8; ENCODER_STATE_SIZE_STEREO];
    let mut encoder =
        Encoder::new(&mut state_buf, SAMPLE_RATE as i32, 2, Application::Audio).unwrap();
    encoder.set_bitrate(64_000).unwrap(); // 64kbps
    encoder.set_complexity(3).unwrap();
    info!("encode: starting");
    loop {
        let (pcm, opus) = join(rx.receive(), tx.send()).await;
        let pcm: &[i16] = bytemuck::cast_slice(pcm.as_slice());
        match encoder.encode(pcm, &mut opus.data) {
            Ok(len) => opus.len = len,
            Err(_e) => {
                error!("encode: failed");
                opus.len = 0;
            }
        }
        rx.receive_done();
        tx.send_done();
    }
}

/// Radio TX — Core 0.  SPI DMA + 1ms timer polls for FIFO refill, DIO0
/// interrupt for PacketSent.
#[embassy_executor::task]
async fn radio_transmit_task(
    mut rx: OpusPacketReceiver,
    mut radio: Sx127xConcrete,
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
        rx.receive_done();
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    // ----- Setup the RP2350 PLL and Clocks and init peripherals
    let mut config = SystemConfig::default();
    config.clocks = ClockConfig::system_freq(CLOCK_HZ).unwrap();
    let p = embassy_rp::init(config);

    // Grab all the pins
    // PCM1808
    let scki_out = p.PIN_22;
    let data_in = p.PIN_26;
    let bck_in = p.PIN_27;
    let lrck_in = p.PIN_28;
    // SX1276 (RFM95W)
    let sck = p.PIN_2;
    let mosi = p.PIN_3;
    let miso = p.PIN_4;
    let cs = p.PIN_5;
    // DIO0 / G0 → PacketSent interrupt (GPIO6)
    let dio0 = Input::new(p.PIN_6, Pull::Down);
    // RST (GPIO7) — passed to radio task for hardware reset before config.
    let rst = Output::new(p.PIN_7, Level::High);

    // ----- Configure the PWM timer peripheral
    let mut pwm_config = PwmConfig::default();
    pwm_config.divider = FixedU16::<U4>::from_num(6.25); // 153.6 MHz / 6.25 = 24.576 MHz
    pwm_config.top = 1; // period = 2 counter ticks → 12.288 MHz
    pwm_config.enable = true;
    let mut pwm = Pwm::new_output_a(p.PWM_SLICE3, scki_out, pwm_config);
    pwm.set_duty_cycle(1).unwrap(); // high for 1 of 2 ticks → 50% duty
    // Prevent Drop from disabling the PWM when main returns
    core::mem::forget(pwm);

    // ----- Configure both I2S peripherals on PIO0 (SM0=input, SM1=output)
    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);

    let i2s_in_prg = PioI2sInputProgram::new(&mut common);
    let i2s_in = I2sInput::new(
        &mut common,
        sm0,
        p.DMA_CH0,
        Irqs,
        data_in,
        bck_in,
        lrck_in,
        &i2s_in_prg,
    );

    // ----- Configure SPI and the Radio
    let mut config = SpiConfig::default();
    config.frequency = 10_000_000; // Could bump to 10 MHz
    let spi = Spi::new(p.SPI0, sck, mosi, miso, p.DMA_CH1, p.DMA_CH2, Irqs, config);
    static SPI_BUS: StaticCell<Spi0Bus> = StaticCell::new();
    let spi_bus = SPI_BUS.init(Mutex::new(spi));
    let cs = Output::new(cs, Level::High);
    let spi_dev = SpiDevice::new(spi_bus, cs);

    // Radio configured in radio_transmit_task (needs async SPI).
    let radio = Sx127x::new(spi_dev);

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
        p.CORE1,
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
        spawner.spawn(i2s_in_task(i2s_in, i2s_opus_tx).unwrap());
        spawner.spawn(radio_transmit_task(encode_radio_rx, radio, dio0, rst).unwrap());
    })
}
