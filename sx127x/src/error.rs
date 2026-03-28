use embedded_hal_async::spi::Error as SpiError;

#[derive(thiserror::Error, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error<E: SpiError> {
    #[error("SPI bus error: {0:?}")]
    BusError(#[from] E),
    #[error("CRC check failed")]
    CrcError,
    #[error("Invalid packet (bad length)")]
    InvalidPacket,
    #[error("Receive timeout — no preamble/sync detected within 1 s")]
    Timeout,
}
