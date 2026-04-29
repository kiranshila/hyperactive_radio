// I2S input — PCM3060 ADC drives BCK + LRCK, we recieve
// Reads 16 bits per channel (MSBs of 24-bit ADC output) in standard I2S format
// (one BCK delay after LRCK edge, MSB first).

use embassy_rp::{
    Peri,
    dma::{Channel, ChannelInstance, InterruptHandler, Transfer},
    interrupt::typelevel::Binding,
    pio::{
        Common, Config as PioConfig, Direction, FifoJoin, Instance, LoadedProgram, PioPin,
        ShiftConfig, ShiftDirection, StateMachine,
        program::{pio_asm, pio_file},
    },
};

pub(crate) struct PioI2sInProgram<'d, PIO: Instance> {
    prg: LoadedProgram<'d, PIO>,
}

impl<'d, PIO: Instance> PioI2sInProgram<'d, PIO> {
    pub(crate) fn new(common: &mut Common<'d, PIO>) -> Self {
        let prg = pio_asm! {
            "start:",
            "   wait 1 pin 2",         // sync: wait for LRCK high
            "sync_l:",
            "   wait 0 pin 2",         // wait for LRCK falling edge (start of L channel)
            "   set y, 1",             // y=1 → currently reading L channel
            "   wait 0 pin 1",         // ensure we are past BCK falling edge
            "   wait 1 pin 1",         // skip the first BCK rising edge (I2S one-cycle delay)
            "read_word:",
            "   set x, 15",            // 16 bits per channel (x is the down-counter)
            "read_bit:",
            "   wait 0 pin 1",         // wait BCK low
            "   wait 1 pin 1",         // wait BCK high — data is valid now
            "   in pins, 1",           // shift DATA bit into ISR
            "   jmp x-- read_bit",     // repeat for all 16 bits
            "   jmp !y sync_l",        // y=0 (R channel done) → re-sync on next LRCK
            "   wait 1 pin 2",         // wait for LRCK rising edge (start of R channel)
            "   set y, 0",             // y=0 → now reading R channel
            "   wait 0 pin 1",
            "   wait 1 pin 1",         // skip first BCK edge of R channel
            "   jmp read_word",
        };
        let prg = common.load_program(&prg.program);
        Self { prg }
    }
}

pub struct I2sInput<'d, P: Instance, const S: usize> {
    dma: Channel<'d>,
    sm: StateMachine<'d, P, S>,
}

impl<'d, P: Instance, const S: usize> I2sInput<'d, P, S> {
    pub(crate) fn new<D: ChannelInstance>(
        common: &mut Common<'d, P>,
        mut sm: StateMachine<'d, P, S>,
        dma: Peri<'d, D>,
        irq: impl Binding<D::Interrupt, InterruptHandler<D>> + 'd,
        data: Peri<'d, impl PioPin>,
        bck: Peri<'d, impl PioPin>,
        lrck: Peri<'d, impl PioPin>,
        program: &PioI2sInProgram<'d, P>,
    ) -> Self {
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
        cfg.fifo_join = FifoJoin::RxOnly;

        sm.set_config(&cfg);
        sm.set_pin_dirs(Direction::In, &[&data, &bck, &lrck]);

        Self {
            dma: Channel::new(dma, irq),
            sm,
        }
    }

    pub fn start(&mut self) {
        self.sm.set_enable(true);
    }

    pub fn read<'b>(&'b mut self, buff: &'b mut [u32]) -> Transfer<'b> {
        self.sm.rx().dma_pull(&mut self.dma, buff, false)
    }
}

//  I2S output — RP2350 reads BCK + LRCK at 64fs ("master mode" config on the DAC)
//
// Each 32-bit DMA word is one stereo sample: L[31:16] | R[15:0].
// PCM3060 slave (24-bit I2S) sees 16 MSBs of its 24-bit window; bottom 8 bits = 0.
//
// PIO clock = SAMPLE_RATE × 64 × 2  (2 PIO instructions per BCK half-cycle)

pub(crate) struct PioI2sOutProgram<'d, PIO: Instance> {
    prg: LoadedProgram<'d, PIO>,
}

impl<'d, PIO: Instance> PioI2sOutProgram<'d, PIO> {
    pub(crate) fn new(common: &mut Common<'d, PIO>) -> Self {
        let prg = pio_file!("src/i2s_out.s");
        let prg = common.load_program(&prg.program);
        Self { prg }
    }
}

pub struct I2sOutput<'d, P: Instance, const S: usize> {
    dma: Channel<'d>,
    sm: StateMachine<'d, P, S>,
}

impl<'d, P: Instance, const S: usize> I2sOutput<'d, P, S> {
    pub(crate) fn new<D: ChannelInstance>(
        common: &mut Common<'d, P>,
        mut sm: StateMachine<'d, P, S>,
        dma: Peri<'d, D>,
        irq: impl Binding<D::Interrupt, InterruptHandler<D>> + 'd,
        data: Peri<'d, impl PioPin>,
        bck: Peri<'d, impl PioPin>,
        lrck: Peri<'d, impl PioPin>,
        program: &PioI2sOutProgram<'d, P>,
    ) -> Self {
        let data = common.make_pio_pin(data);
        let bck = common.make_pio_pin(bck);
        let lrck = common.make_pio_pin(lrck);

        let mut cfg = PioConfig::default();
        cfg.use_program(&program.prg, &[]);

        // so that all the pin numbers agree, makes things nice
        cfg.set_in_pins(&[&data, &bck, &lrck]);
        cfg.set_out_pins(&[&data, &bck, &lrck]);
        cfg.set_set_pins(&[&data, &bck, &lrck]);

        cfg.shift_out = ShiftConfig {
            threshold: 32,
            direction: ShiftDirection::Left,
            auto_fill: true,
        };
        cfg.fifo_join = FifoJoin::TxOnly;

        sm.set_config(&cfg);
        // NOTE: data will be set to out in the program itself
        sm.set_pin_dirs(Direction::In, &[&data, &bck, &lrck]);

        Self {
            dma: Channel::new(dma, irq),
            sm,
        }
    }

    pub fn start(&mut self) {
        self.sm.set_enable(true);
    }

    pub fn write<'b>(&'b mut self, buff: &'b [u32]) -> Transfer<'b> {
        self.sm.tx().dma_push(&mut self.dma, buff, false)
    }
}
