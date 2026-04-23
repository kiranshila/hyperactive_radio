#![no_std]

use device_driver::AsyncRegisterInterface;
use embedded_hal_async::i2c::I2c;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Format {
    I2s24Bit = 0,
    LeftJustified24Bit = 1,
    RightJustified24Bit = 2,
    RightJustified16Bit = 3,
}

impl From<u8> for Format {
    fn from(v: u8) -> Self {
        match v & 0b11 {
            0 => Self::I2s24Bit,
            1 => Self::LeftJustified24Bit,
            2 => Self::RightJustified24Bit,
            _ => Self::RightJustified16Bit,
        }
    }
}

impl From<Format> for u8 {
    fn from(v: Format) -> Self {
        v as u8
    }
}

// ── Register map ──────────────────────────────────────────────────────────────

device_driver::create_device!(device_name: Device, manifest: "device.yaml");

// ── Bus interface ──────────────────────────────────────────────────────────-──

pub struct I2cInterface<BUS> {
    i2c: BUS,
    addr: u8,
}

impl<BUS> I2cInterface<BUS> {
    /// ADR is the state of the ADR pin to control the I2C address
    pub fn new(i2c: BUS, adr: bool) -> Self {
        let addr = 0b1000110 | adr as u8;
        Self { i2c, addr }
    }
}

impl<BUS: I2c> AsyncRegisterInterface for I2cInterface<BUS> {
    type Error = BUS::Error;
    type AddressType = u8;

    async fn write_register(
        &mut self,
        address: u8,
        _size_bits: u32,
        data: &[u8],
    ) -> Result<(), Self::Error> {
        self.i2c.write(self.addr as u8, &[address, data[0]]).await
    }

    async fn read_register(
        &mut self,
        address: u8,
        _size_bits: u32,
        data: &mut [u8],
    ) -> Result<(), Self::Error> {
        self.i2c.write_read(self.addr as u8, &[address], data).await
    }
}

// -- Top-level driver
pub struct Pcm3060<BUS> {
    pub device: Device<I2cInterface<BUS>>,
}

impl<BUS: I2c> Pcm3060<BUS> {
    pub fn new(i2c: BUS, adr: bool) -> Self {
        Self {
            device: Device::new(I2cInterface::new(i2c, adr)),
        }
    }

    // -- High-level interface
    pub async fn reset(&mut self) -> Result<(), BUS::Error> {
        self.device
            .reg_64()
            .modify_async(|x| x.set_mrst(false))
            .await?;
        Ok(())
    }

    /// Initialize the DAC
    pub async fn dac_init(&mut self) -> Result<(), BUS::Error> {
        self.device
            .reg_64()
            .modify_async(|x| {
                // Set output to single-ended
                x.set_s_e(true);
                // Normal operation
                x.set_dapsv(false);
            })
            .await?;
        self.device
            .reg_69()
            .modify_async(|x| {
                // De-emphasis off — our source audio is not pre-emphasized
                x.set_dmc(false);
            })
            .await?;
        Ok(())
    }

    /// Initialize the ADC
    pub async fn adc_init(&mut self) -> Result<(), BUS::Error> {
        self.device
            .reg_64()
            .modify_async(|x| {
                x.set_adpsv(false);
            })
            .await?;
        self.device
            .reg_72()
            .modify_async(|x| {
                // Master mode here matches the PCM1808
                // Where the PCM is responsible for generating the clocks
                x.set_m_ns(AdcMode::Master256Fs);
            })
            .await?;
        Ok(())
    }
}
