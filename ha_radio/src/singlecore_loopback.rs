#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

mod shared;
use {
    crate::shared::{
        CLOCK_HZ, FRAME_SAMPLES, OPUS_BUF_SIZE, OpusPacket, PackedAudioFrame, SAMPLE_RATE,
    },
    defmt::*,
    defmt_rtt as _,
    embassy_executor::Spawner,
    embassy_futures::join::join,
    embassy_rp::{
        Peri, bind_interrupts,
        clocks::ClockConfig,
        config::Config as SystemConfig,
        dma::{
            self, Channel as DmaChannel, ChannelInstance, InterruptHandler as DmaInterruptHandler,
            Transfer,
        },
        interrupt::typelevel::Binding,
        peripherals::{DMA_CH0, DMA_CH1, PIO0},
        pio::{
            Common, Config as PioConfig, Direction, FifoJoin, Instance as PioInstance,
            InterruptHandler as PioInterruptHandler, LoadedProgram, Pio, PioPin, ShiftConfig,
            ShiftDirection, StateMachine, program::pio_asm,
        },
        pio_programs::i2s::{PioI2sOut, PioI2sOutProgram},
        pwm::{Config as PwmConfig, Pwm, SetDutyCycle},
    },
    embassy_sync::{
        blocking_mutex::raw::CriticalSectionRawMutex,
        zerocopy_channel::{Channel, Receiver, Sender},
    },
    embassy_time::Timer,
    embedded_opus::{
        Application, DECODER_STATE_SIZE_STEREO, Decoder, ENCODER_STATE_SIZE_STEREO, Encoder,
    },
    fixed::{FixedU16, types::extra::U4},
    panic_probe as _,
    static_cell::StaticCell,
};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => DmaInterruptHandler<DMA_CH0>, DmaInterruptHandler<DMA_CH1>;
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
});

// ----- I2S Input Program
// This is different from the "stock" program as we're not
// generating the bit clock or channel select (I tried, it
// didn't work). This makes the program actually more simple.
// NOTE: This is the "I2S mode" FMT=0 from the PCM1808 where
// the bck cycles a null bit after LRCK state changes. This is
// notably different from "left-justified"

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

        // Pins must be consecutive GPIOs for set_in_pins.
        // pin 0 = DATA (GPIO 26), pin 1 = BCK (GPIO 27), pin 2 = LRCK (GPIO 28)
        // `in pins, 1` reads pin 0 (DATA).

        // Configure the PIO peripheral
        let mut cfg = PioConfig::default();
        // Load program, no side-set (output) pins
        cfg.use_program(&program.prg, &[]);
        // Set input pins — must be in consecutive GPIO order
        cfg.set_in_pins(&[&data, &bck, &lrck]);
        // ShiftDirection::Left: `in pins, 1` does ISR = (ISR << 1) | bit.
        // After 16 L bits then 16 R bits, the u32 = (left << 16) | right.
        // On little-endian ARM, this stores as [R_lo, R_hi, L_lo, L_hi].
        // Reinterpreted as i16 via bytemuck: [right, left] per sample pair.
        // Opus sees channel 0 = R, channel 1 = L (swapped from convention,
        // but consistent through the encode→decode round-trip).
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
// Four-stage pipeline:
// Task 1: Capture I2S from PCM1808
// Task 2: Encode with Opus (This will block for ~18 ms)
// Task 3: Decode with Opus (This will block for ~5 ms)
// Task 4: Transmit I2S to PCM1502A
//
// Channels connect the four tasks

type PackedAudioFrameChannel = Channel<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameSender = Sender<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameReceiver = Receiver<'static, CriticalSectionRawMutex, PackedAudioFrame>;

type OpusPacketChannel = Channel<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketSender = Sender<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketReceiver = Receiver<'static, CriticalSectionRawMutex, OpusPacket>;

#[embassy_executor::task]
async fn i2s_in_task(mut i2s: I2sInput<'static, PIO0, 0>, mut tx: PackedAudioFrameSender) {
    // Start the I2S input
    i2s.start();
    info!("i2s in: started");
    loop {
        // Grab the next buffer from the channel.
        // If this blocks, the pipeline is behind and the PIO FIFO
        // will overflow (only 166µs of headroom at 48kHz with 8-deep FIFO).
        let t = embassy_time::Instant::now();
        let buf = tx.send().await;
        let wait_us = t.elapsed().as_micros();
        if wait_us > 100 {
            warn!("i2s in: stalled {}us waiting for free buffer", wait_us);
        }
        // Start the DMA transfer directly into the buffer
        i2s.read(buf).await;
        // Notify the channel this buffer has good data in it
        tx.send_done();
    }
}

#[embassy_executor::task]
async fn opus_encode_task(mut rx: PackedAudioFrameReceiver, mut tx: OpusPacketSender) {
    // Create the opus state memory
    let mut state_buf = [0u8; ENCODER_STATE_SIZE_STEREO];
    let mut encoder =
        Encoder::new(&mut state_buf, SAMPLE_RATE as i32, 2, Application::Audio).unwrap();
    encoder.set_bitrate(64_000).unwrap(); // 64kbps
    encoder.set_complexity(3).unwrap(); // Middle of the road, benched ok
    info!("encode: starting");
    loop {
        // Acquire both slots concurrently to avoid holding the PCM buffer
        // while waiting for a free packet slot
        let (pcm, opus) = join(rx.receive(), tx.send()).await;
        // bytemuck reinterprets [u32] as [i16] in-place (zero-copy).
        // Little-endian: each u32 (left<<16)|right becomes [right, left] as i16.
        // Sample VALUES are correct native-endian signed integers.
        // Channel order is [R, L, R, L, ...] — swapped, but Opus takes
        // native-endian i16 (not big-endian), so values are correct.
        // The decode side uses the same layout, making the round-trip consistent.
        let pcm: &[i16] = bytemuck::cast_slice(pcm.as_slice());
        // Encode into the packet buffer (sized to the radio payload limit).
        match encoder.encode(pcm, &mut opus.data) {
            Ok(len) => opus.len = len,
            Err(_e) => {
                error!("encode: failed");
                opus.len = 0;
            }
        }
        // Release PCM buffer first so I2S DMA can reclaim it
        rx.receive_done();
        tx.send_done();
    }
}

#[embassy_executor::task]
async fn opus_decode_task(mut rx: OpusPacketReceiver, mut tx: PackedAudioFrameSender) {
    // Set up the decoder with its state
    let mut state_buf = [0u8; DECODER_STATE_SIZE_STEREO];
    let mut decoder = Decoder::new(&mut state_buf, SAMPLE_RATE as i32, 2).unwrap();
    info!("decode: starting");
    loop {
        // Acquire both slots
        let (opus, pcm) = join(rx.receive(), tx.send()).await;
        let pcm: &mut [i16] = bytemuck::cast_slice_mut(pcm);
        // Decode into PCM buffer
        match decoder.decode(&opus.data[0..opus.len], pcm, false) {
            Ok(_len) => {}
            Err(_e) => {
                error!("decode: failed");
                decoder.plc(pcm).unwrap();
            }
        }
        // Release buffers
        rx.receive_done();
        tx.send_done();
    }
}

#[embassy_executor::task]
async fn i2s_out_task(mut i2s: PioI2sOut<'static, PIO0, 1>, mut rx: PackedAudioFrameReceiver) {
    // Start the I2S output
    i2s.start();
    info!("i2s out: started");
    loop {
        // Grab the next buffer from the channel
        let buf = rx.receive().await;
        // Start the DMA transfer directly into the buffer in the "background"
        i2s.write(buf).await;
        // Notify the channel this buffer has been used and can be recycled
        rx.receive_done();
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // ----- Setup the RP2350 PLL and Clocks and init peripherals
    // We run the clock a little faster than stock at 153.6 MHz
    // such that we can cleanly PLL down to the I2S clock of 12.288 MHz.
    // We need this as we will run at PCM1808  at a sample freq of 48 kHz
    // which requires a system clock freq of 256 fs, or 12.288 MHz.
    // We could use an external clock generator for this, but this *is* one
    // fewer part.
    let mut config = SystemConfig::default();
    config.clocks = ClockConfig::system_freq(CLOCK_HZ).unwrap();
    let p = embassy_rp::init(config);

    // Grab all the pins
    // PCM1808
    let scki_out = p.PIN_22;
    let data_in = p.PIN_26;
    let bck_in = p.PIN_27;
    let lrck_in = p.PIN_28;
    // PCM1502A
    let data_out = p.PIN_5;
    let bck_out = p.PIN_6;
    let lrck_out = p.PIN_7;

    // ----- Configure the PWM timer peripheral
    // Output the PCM1808 system clock (SCKI) at 12.288 MHz.
    // CLOCK_HZ / I2S_CLK_HZ = 153_600_000 / 12_288_000 = 12.5 (exact).
    // With divider=1 the PWM counter runs at CLOCK_HZ, so
    // top = 12 gives a period of 13 ticks → 153.6 MHz / 13 ≈ 11.815 MHz (wrong).
    // Instead: divider = 12.5, top = 0 → period = 1 tick at 12.288 MHz.
    // Duty = 0 with top = 0 gives a narrow pulse; that's fine as SCKI just
    // needs edges, but for a clean square wave we use top = 1, duty = 1:
    // divider = 6.25 → counter at 24.576 MHz, period = 2 ticks → 12.288 MHz.
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
        mut common,
        sm0,
        sm1,
        ..
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

    let i2s_out_prg = PioI2sOutProgram::new(&mut common);
    let i2s_out = PioI2sOut::new(
        &mut common,
        sm1,
        p.DMA_CH1,
        Irqs,
        data_out,
        bck_out,
        lrck_out,
        SAMPLE_RATE,
        16,
        &i2s_out_prg,
    );

    // Wait for 8960/fs for the PCM1808 to output valid data
    Timer::after_millis(200).await;

    // ----- Set up the three zero-copy channels
    // The first and last channel will exist on both sides, the middle channel represents the radio

    // I2S -> OPUS
    static I2S_OPUS_BUF: StaticCell<[PackedAudioFrame; 2]> = StaticCell::new();
    let i2s_opus_buf = I2S_OPUS_BUF.init([[0; FRAME_SAMPLES / 2]; 2]);
    static I2S_OPUS_CHAN: StaticCell<PackedAudioFrameChannel> = StaticCell::new();
    let i2s_opus_chan = I2S_OPUS_CHAN.init(Channel::new(i2s_opus_buf));
    let (i2s_opus_tx, i2s_opus_rx) = i2s_opus_chan.split();

    // OPUS -> OPUS
    static ENCODE_DECODE_BUF: StaticCell<[OpusPacket; 2]> = StaticCell::new();
    let encode_decode_buf = ENCODE_DECODE_BUF.init(
        [OpusPacket {
            data: [0; OPUS_BUF_SIZE],
            len: 0,
        }; 2],
    );
    static ENCODE_DECODE_CHAN: StaticCell<OpusPacketChannel> = StaticCell::new();
    let encode_decode_chan = ENCODE_DECODE_CHAN.init(Channel::new(encode_decode_buf));
    let (encode_decode_tx, encode_decode_rx) = encode_decode_chan.split();

    // OPUS -> I2S
    static OPUS_I2S_BUF: StaticCell<[PackedAudioFrame; 2]> = StaticCell::new();
    let opus_i2s_buf = OPUS_I2S_BUF.init([[0; FRAME_SAMPLES / 2]; 2]);
    static OPUS_I2S_CHAN: StaticCell<PackedAudioFrameChannel> = StaticCell::new();
    let opus_i2s_chan = OPUS_I2S_CHAN.init(Channel::new(opus_i2s_buf));
    let (opus_i2s_tx, opus_i2s_rx) = opus_i2s_chan.split();

    // ----- Spawn all the tasks
    spawner.spawn(i2s_in_task(i2s_in, i2s_opus_tx).unwrap());
    spawner.spawn(opus_encode_task(i2s_opus_rx, encode_decode_tx).unwrap());
    spawner.spawn(opus_decode_task(encode_decode_rx, opus_i2s_tx).unwrap());
    spawner.spawn(i2s_out_task(i2s_out, opus_i2s_rx).unwrap());
}
