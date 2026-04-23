//! All the boilerplate for initializing the board

#![no_std]
#![no_main]

pub mod consts;
pub mod i2s;

use crate::{
    consts::SAMPLE_RATE,
    i2s::{I2sInput, I2sOutput, PioI2sInProgram, PioI2sOutProgram},
};
use core::marker::PhantomData;
use embassy_embedded_hal::shared_bus::asynch::{i2c::I2cDevice, spi::SpiDevice};
use embassy_rp::{
    Peri,
    clocks::ClockConfig,
    dma::{ChannelInstance, InterruptHandler as DmaInterruptHandler},
    gpio::{Input, Level, Output, Pull},
    i2c::{
        Async as I2cAsync, Config as I2cConfig, I2c, Instance as I2cInstance,
        InterruptHandler as I2cInterruptHandler,
    },
    interrupt::typelevel::Binding,
    peripherals::{CORE1, DMA_CH0, DMA_CH1, DMA_CH2, DMA_CH3, I2C0, PIO0, SPI0},
    pio::{Instance as PioInstance, InterruptHandler as PioInterruptHandler, Pio},
    spi::{Async as SpiAsync, Config as SpiConfig, Spi},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use pcm3060::Pcm3060;
use static_cell::StaticCell;
use sx127x::Sx127x;

type I2c0 = I2c<'static, I2C0, I2cAsync>;
type I2c0Bus = Mutex<CriticalSectionRawMutex, I2c0>;
type I2c0Device = I2cDevice<'static, CriticalSectionRawMutex, I2c0>;
type Spi0 = Spi<'static, SPI0, SpiAsync>;
type Spi0Bus = Mutex<CriticalSectionRawMutex, Spi0>;
type Spi0Device = SpiDevice<'static, CriticalSectionRawMutex, Spi0, Output<'static>>;

pub type I2sInputPio = I2sInput<'static, PIO0, 0>;
pub type I2sOutputPio = I2sOutput<'static, PIO0, 1>;
pub type Pcm3060Board = Pcm3060<I2c0Device>;
pub type Sx127xBoard = Sx127x<Spi0Device>;

const CLOCK_FREQ_HZ: u32 = 240_000_000;

static I2C0_BUS: StaticCell<I2c0Bus> = StaticCell::new();

/// Represents all the peripherals and pins as wired on the production board
pub struct Board<IRQS> {
    /// Headphone/Line Out Detect
    /// Active-low when a plug is inserted.
    pub out_det: Input<'static>,

    /// Radio GPIO
    pub radio_rst: Output<'static>,
    pub radio_d0: Input<'static>,
    pub radio_d1: Input<'static>,
    pub radio_d2: Input<'static>,
    pub radio_d3: Input<'static>,
    pub radio_d4: Input<'static>,
    pub radio_d5: Input<'static>,

    /// Rotary Encoder
    // pub encoder_a: Peri<'static, PIN_13>,
    // pub encoder_b: Peri<'static, PIN_14>,
    // pub encoder_sw: Peri<'static, PIN_15>,

    /// Audio Codec Software Control
    pub codec: Pcm3060Board,

    /// Audio amp shutdown pin
    pub amp_nshdn: Output<'static>,

    /// I2S PIO Programs
    pub i2s_out: I2sOutputPio,
    pub i2s_in: I2sInputPio,

    /// Radio
    pub radio: Sx127xBoard,

    /// IRQ Marker
    irqs: PhantomData<IRQS>,

    /// Core 1
    pub core_1: Peri<'static, CORE1>,
}

impl<IRQS> Board<IRQS>
where
    IRQS: Binding<<I2C0 as I2cInstance>::Interrupt, I2cInterruptHandler<I2C0>>
        + Binding<<PIO0 as PioInstance>::Interrupt, PioInterruptHandler<PIO0>>
        + Binding<<DMA_CH0 as ChannelInstance>::Interrupt, DmaInterruptHandler<DMA_CH0>>
        + Binding<<DMA_CH1 as ChannelInstance>::Interrupt, DmaInterruptHandler<DMA_CH1>>
        + Binding<<DMA_CH2 as ChannelInstance>::Interrupt, DmaInterruptHandler<DMA_CH2>>
        + Binding<<DMA_CH3 as ChannelInstance>::Interrupt, DmaInterruptHandler<DMA_CH3>>
        + 'static,
{
    pub fn new(mut config: embassy_rp::config::Config, irqs: IRQS) -> Self {
        config.clocks = ClockConfig::system_freq(CLOCK_FREQ_HZ).unwrap();
        let p = embassy_rp::init(config);

        // Amp shutdown pin
        let amp_nshdn = Output::new(p.PIN_22, Level::Low);

        // Setup I2C
        let mut i2c_cfg = I2cConfig::default();
        i2c_cfg.frequency = 400_000;
        let i2c = I2c::new_async(p.I2C0, p.PIN_21, p.PIN_20, irqs, i2c_cfg);
        let i2c_bus = I2C0_BUS.init(Mutex::new(i2c));

        // Setup Codec
        let codec = Pcm3060::new(I2cDevice::new(i2c_bus), false);

        // Setup I2S
        let Pio {
            mut common,
            sm0,
            sm1,
            ..
        } = Pio::new(p.PIO0, irqs);

        let i2s_in_prg = PioI2sInProgram::new(&mut common);
        let i2s_in = I2sInput::new(
            &mut common,
            sm0,
            p.DMA_CH0,
            irqs,
            p.PIN_26,
            p.PIN_27,
            p.PIN_28,
            &i2s_in_prg,
        );

        let i2s_out_prg = PioI2sOutProgram::new(&mut common);
        let i2s_out = I2sOutput::new(
            &mut common,
            sm1,
            p.DMA_CH1,
            irqs,
            p.PIN_16,
            p.PIN_17,
            p.PIN_18,
            SAMPLE_RATE,
            &i2s_out_prg,
        );

        // Output jack detection
        let out_det = Input::new(p.PIN_1, Pull::Up);

        // SPI for radio
        let mut config = SpiConfig::default();
        config.frequency = 10_000_000;
        let spi = Spi::new(
            p.SPI0, p.PIN_2, p.PIN_3, p.PIN_4, p.DMA_CH2, p.DMA_CH3, irqs, config,
        );
        static SPI_BUS: StaticCell<Spi0Bus> = StaticCell::new();
        let spi_bus = SPI_BUS.init(Mutex::new(spi));
        let cs = Output::new(p.PIN_5, Level::High);
        let spi_dev = SpiDevice::new(spi_bus, cs);

        // Radio
        let radio = Sx127x::new(spi_dev);

        // Other GPIO
        let radio_rst = Output::new(p.PIN_6, Level::High);
        let radio_d0 = Input::new(p.PIN_7, Pull::Down);
        let radio_d1 = Input::new(p.PIN_8, Pull::Down);
        let radio_d2 = Input::new(p.PIN_9, Pull::Down);
        let radio_d3 = Input::new(p.PIN_10, Pull::Down);
        let radio_d4 = Input::new(p.PIN_11, Pull::Down);
        let radio_d5 = Input::new(p.PIN_12, Pull::Down);

        Self {
            codec,
            amp_nshdn,
            out_det,
            radio,
            i2s_out,
            i2s_in,
            irqs: PhantomData,
            radio_rst,
            radio_d0,
            radio_d1,
            radio_d2,
            radio_d3,
            radio_d4,
            radio_d5,
            core_1: p.CORE1,
        }
    }
}
