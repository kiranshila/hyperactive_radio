//! PIO backed quadrature encoder. Stolen from Embassy and reworked for
//! finer-grained counts.

use embassy_rp::{
    Peri,
    gpio::Pull,
    pio::{
        Common, Config, Direction as PioDirection, FifoJoin, Instance, PioPin, ShiftDirection,
        StateMachine, program::pio_file,
    },
    pio_programs::clock_divider::calculate_pio_clock_divider,
};

/// Pio Backed quadrature encoder reader
pub struct Encoder<'d, T: Instance, const SM: usize, const MIN: isize, const MAX: isize> {
    sm: StateMachine<'d, T, SM>,
    pos: isize,
}

impl<'d, T: Instance, const SM: usize, const MIN: isize, const MAX: isize>
    Encoder<'d, T, SM, MIN, MAX>
{
    /// Configure a state machine with the loaded [PioEncoderProgram]
    pub fn new(
        common: &mut Common<'d, T>,
        mut sm: StateMachine<'d, T, SM>,
        pin_a: Peri<'d, impl PioPin>,
        pin_b: Peri<'d, impl PioPin>,
    ) -> Self {
        let prg = pio_file!("src/encoder.s");
        let prg = common.load_program(&prg.program);

        let mut pin_a = common.make_pio_pin(pin_a);
        let mut pin_b = common.make_pio_pin(pin_b);
        pin_a.set_pull(Pull::Up);
        pin_b.set_pull(Pull::Up);
        sm.set_pin_dirs(PioDirection::In, &[&pin_a, &pin_b]);

        let mut cfg = Config::default();
        cfg.set_in_pins(&[&pin_a, &pin_b]);
        cfg.fifo_join = FifoJoin::RxOnly;
        cfg.shift_in.direction = ShiftDirection::Left;

        // Target 12.5 KHz PIO clock
        cfg.clock_divider = calculate_pio_clock_divider(12_500);

        cfg.use_program(&prg, &[]);
        sm.set_config(&cfg);
        sm.set_enable(true);
        Self { sm, pos: MIN }
    }

    /// Read a single count from the encoder. Should be called often enough to
    /// catch updates.
    pub async fn poll(&mut self) -> Direction {
        match self.sm.rx().wait_pull().await {
            // tweaked to match the encoder we have
            0 | 3 => {
                self.pos = MAX.min(self.pos + 1);
                Direction::Clockwise(self.pos)
            }

            1 | 2 => {
                self.pos = MIN.max(self.pos - 1);
                Direction::CounterClockwise(self.pos)
            }

            _ => Direction::NoChange(self.pos),
        }
    }

    pub fn pos(&self) -> isize {
        self.pos
    }

    pub fn min(&self) -> isize {
        MIN
    }

    pub fn max(&self) -> isize {
        MAX
    }
}

/// Encoder Count Direction
pub enum Direction {
    /// Encoder turned clockwise to the given position
    Clockwise(isize),
    /// Encoder turned counterclockwise to the given position
    CounterClockwise(isize),
    /// Encoder did not change position
    NoChange(isize),
}
