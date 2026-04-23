// I2S input — PCM3060 ADC drives BCK + LRCK, we recieve
// Reads 16 bits per channel (MSBs of 24-bit ADC output) in standard I2S format
// (one BCK delay after LRCK edge, MSB first).

use embassy_rp::{
    Peri,
    dma::{Channel, ChannelInstance, InterruptHandler, Transfer},
    interrupt::typelevel::Binding,
    pio::{
        Common, Config as PioConfig, Direction, FifoJoin, Instance, LoadedProgram, PioPin,
        ShiftConfig, ShiftDirection, StateMachine, program::pio_asm,
    },
    pio_programs::clock_divider::calculate_pio_clock_divider,
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

//  I2S output — RP2350 drives BCK + LRCK at 64fs ("master mode")
//
// Side-set: sidebit0 = BCK, sidebit1 = LRCK  (side 0bLB)
//
// Each 32-bit DMA word is one stereo sample: L[31:16] | R[15:0].
// 16 data bits + 16 zero-pad bits per channel = 32 BCK cycles per half = 64fs.
// PCM3060 slave (24-bit I2S) sees 16 MSBs of its 24-bit window; bottom 8 bits = 0.
//
// PIO clock = SAMPLE_RATE × 64 × 2  (2 PIO instructions per BCK half-cycle)

pub(crate) struct PioI2sOutProgram<'d, PIO: Instance> {
    prg: LoadedProgram<'d, PIO>,
}

impl<'d, PIO: Instance> PioI2sOutProgram<'d, PIO> {
    pub(crate) fn new(common: &mut Common<'d, PIO>) -> Self {
        let prg = pio_asm!(
            ".side_set 2",
            // --- Left channel (LRCK=0)
            "    mov x, y              side 0b01", // BCK=1 — I2S 1-cycle delay before MSB
            "left_data:",
            "    out pins, 1           side 0b00", // BCK=0, output bit
            "    jmp x-- left_data     side 0b01", // BCK=1
            "    out pins, 1           side 0b00", // BCK=0, 16th (last) data bit
            // 16 zero-pad BCK cycles to reach 64fs
            "    set x, 14             side 0b01", // BCK=1
            "left_pad:",
            "    set pins, 0           side 0b00", // BCK=0, DIN=0
            "    jmp x-- left_pad      side 0b01", // BCK=1
            "    set pins, 0           side 0b10", // BCK=0, LRCK=1 — right channel
            // --- Right channel (LRCK=1)
            "    mov x, y              side 0b11", // BCK=1 — 1-cycle delay
            "right_data:",
            "    out pins, 1           side 0b10", // BCK=0, output bit
            "    jmp x-- right_data    side 0b11", // BCK=1
            "    out pins, 1           side 0b10", // BCK=0, 16th data bit
            "    set x, 14             side 0b11", // BCK=1
            "right_pad:",
            "    set pins, 0           side 0b10", // BCK=0
            "    jmp x-- right_pad     side 0b11", // BCK=1
            "    set pins, 0           side 0b00", // BCK=0, LRCK=0 — next left
        );
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
        sample_rate: u32,
        program: &PioI2sOutProgram<'d, P>,
    ) -> Self {
        let data = common.make_pio_pin(data);
        let bck = common.make_pio_pin(bck);
        let lrck = common.make_pio_pin(lrck);

        let mut cfg = PioConfig::default();
        cfg.use_program(&program.prg, &[&bck, &lrck]);
        cfg.set_out_pins(&[&data]);
        cfg.set_set_pins(&[&data]);
        // BCK = 64fs → PIO clock = sample_rate × 64 × 2
        cfg.clock_divider = calculate_pio_clock_divider(sample_rate * 64 * 2);
        cfg.shift_out = ShiftConfig {
            threshold: 32,
            direction: ShiftDirection::Left,
            auto_fill: true,
        };
        cfg.fifo_join = FifoJoin::TxOnly;

        sm.set_config(&cfg);
        sm.set_pin_dirs(Direction::Out, &[&data, &bck, &lrck]);

        // y = bit_depth - 2 = 14 for 16-bit
        unsafe { sm.set_y(14) };

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
