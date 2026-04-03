#![no_std]

use crate::regs::*;
use embedded_hal_async::spi::SpiDevice;
mod error;
mod regs;

#[cfg(feature = "defmt")]
use defmt::*;

/// Crystal oscillator frequency (Hz).
const FXOSC: u32 = 32_000_000;

// ── Public re-exports ────────────────────────────────────────────────────────

pub use error::Error;
pub use regs::ModulationShaping;

// ── Top-level driver ─────────────────────────────────────────────────────────

pub struct Sx127x<S> {
    pub device: Device<DeviceInterface<S>>,
}

impl<S: SpiDevice> Sx127x<S> {
    pub fn new(spi: S) -> Self {
        Self {
            device: Device::new(DeviceInterface::new(spi)),
        }
    }
}

// ── GFSK TX ──────────────────────────────────────────────────────────────────

/// Configuration for GFSK transmit mode.
pub struct GfskConfig {
    /// Carrier frequency in Hz (e.g. `915_000_000`).
    pub frequency_hz: u32,
    /// Air bitrate in bps (e.g. `100_000`).
    pub bitrate_bps: u32,
    /// Peak frequency deviation in Hz. Carson's rule bandwidth ≈ 2*(fdev + bitrate/2).
    pub fdev_hz: u32,
    /// Output power in dBm.
    ///   0..=13  → RFO pin (up to +13 dBm)
    ///   14..=17 → PA_BOOST pin (up to +17 dBm)
    ///   18..=20 → PA_BOOST + high-power mode via PaDac (up to +20 dBm)
    pub tx_power_dbm: i8,
    /// Gaussian pulse shaping applied to the FSK modulator.
    pub modulation_shaping: ModulationShaping,
    /// 4-byte sync word — must match `GfskRxConfig::sync_word` exactly.
    pub sync_word: [u8; 4],
}

impl<S: SpiDevice> Sx127x<S> {
    /// Poll IrqFlags1.ModeReady until the requested mode is active.
    /// Times out after 100 ms to prevent hanging on a wedged radio.
    async fn wait_mode_ready(&mut self) -> Result<(), Error<S::Error>> {
        let mut polls = 0u32;
        loop {
            let flags = self.device.irq_flags_1().read_async().await?;
            if flags.mode_ready() {
                if polls > 0 {
                    #[cfg(feature = "defmt")]
                    debug!("mode_ready after {} polls", polls);
                }
                return Ok(());
            }
            polls += 1;
            if polls >= 10_000 {
                #[cfg(feature = "defmt")]
                debug!(
                    "wait_mode_ready timeout: mode_ready={} rx_ready={} tx_ready={} pll_lock={}",
                    flags.mode_ready(),
                    flags.rx_ready(),
                    flags.tx_ready(),
                    flags.pll_lock(),
                );
                return Err(Error::Timeout);
            }
        }
    }

    /// Clear all writable IRQ flags by writing 1s to the flag bits.
    async fn clear_irq_flags(&mut self) -> Result<(), Error<S::Error>> {
        // IrqFlags1: clear rssi, preamble_detect, sync_address_match
        self.device
            .irq_flags_1()
            .write_async(|w| {
                w.set_rssi(true);
                w.set_preamble_detect(true);
                w.set_sync_address_match(true);
            })
            .await?;
        // IrqFlags2: clear all writable flags
        self.device
            .irq_flags_2()
            .write_async(|w| {
                w.set_fifo_overrun(true);
                w.set_low_bat(true);
            })
            .await?;
        Ok(())
    }

    /// Configure the radio for GFSK transmit.
    ///
    /// Leaves the radio in Standby mode, ready for [`transmit`](Self::transmit)
    /// or [`transmit_continuous`](Self::transmit_continuous).
    pub async fn configure_gfsk_tx(&mut self, cfg: &GfskConfig) -> Result<(), Error<S::Error>> {
        // Two-step Sleep entry — same LoRa-stuck guard as configure_gfsk_rx.
        // Write 1: Mode → Sleep (preserves LongRangeMode if chip was in LoRa non-Sleep).
        // Write 2: LongRangeMode=0 now takes effect (chip is guaranteed in Sleep).
        self.device
            .op_mode()
            .write_async(|w| {
                w.set_modulation_type(ModulationType::FskOok);
                w.set_low_frequency_mode_on(false);
                w.set_mode(Mode::Sleep);
            })
            .await?;
        self.device
            .op_mode()
            .write_async(|w| {
                w.set_modulation_type(ModulationType::FskOok);
                w.set_low_frequency_mode_on(false);
                w.set_mode(Mode::Sleep);
            })
            .await?;

        // Carrier frequency: Frf = frequency_hz * 2^19 / Fxosc
        let frf = ((cfg.frequency_hz as u64) << 19) / FXOSC as u64;
        self.device
            .frf()
            .write_async(|w| w.set_value(frf as u32))
            .await?;

        // Bitrate: RegBitrate = Fxosc / bitrate_bps
        // Fine-tune with BitRateFrac: effective_bitrate = Fxosc / (BitRate + BitRateFrac/16)
        let bitrate_full = (FXOSC as u64 * 16) / cfg.bitrate_bps as u64;
        let bitrate_int = (bitrate_full / 16) as u16;
        let bitrate_frac = (bitrate_full % 16) as u8;
        self.device
            .bitrate()
            .write_async(|w| w.set_value(bitrate_int))
            .await?;
        self.device
            .bit_rate_frac()
            .write_async(|w| w.set_value(bitrate_frac))
            .await?;

        // Frequency deviation: Fdev = fdev_hz * 2^19 / Fxosc
        let fdev = ((cfg.fdev_hz as u64) << 19) / FXOSC as u64;
        self.device
            .fdev()
            .write_async(|w| w.set_value(fdev as u16))
            .await?;

        // Modulation shaping (Gaussian filter).
        self.device
            .pa_ramp_mod_shape()
            .modify_async(|w| {
                w.set_modulation_shaping(cfg.modulation_shaping);
            })
            .await?;

        // Sync word (4 bytes, must match RX exactly).
        // Set explicitly — probe-rs soft resets leave radio registers dirty.
        self.device
            .sync_config()
            .write_async(|w| {
                w.set_sync_on(true);
                w.set_sync_size(3); // 4 bytes
            })
            .await?;
        self.device
            .sync_value_1()
            .write_async(|w| w.set_value(cfg.sync_word[0]))
            .await?;
        self.device
            .sync_value_2()
            .write_async(|w| w.set_value(cfg.sync_word[1]))
            .await?;
        self.device
            .sync_value_3()
            .write_async(|w| w.set_value(cfg.sync_word[2]))
            .await?;
        self.device
            .sync_value_4()
            .write_async(|w| w.set_value(cfg.sync_word[3]))
            .await?;

        // Packet config: variable length, NRZ (no DC encoding), CRC on, packet mode.
        // Written explicitly — soft resets leave radio registers dirty.
        self.device
            .packet_config_1()
            .write_async(|w| {
                w.set_packet_format(true); // variable length
                w.set_crc_on(true);
                // dc_free = NRZ (0), address_filtering = none (0): zeroed by write
            })
            .await?;
        self.device
            .packet_config_2()
            .write_async(|w| w.set_data_mode(true)) // packet mode (not continuous)
            .await?;

        // PA, output power, and PaDac for high-power mode.
        let (pa_select, output_power, max_power, high_power) = gfsk_pa_settings(cfg.tx_power_dbm);
        self.device
            .pa_config()
            .write_async(|w| {
                w.set_pa_select(pa_select);
                w.set_max_power(max_power);
                w.set_output_power(output_power);
            })
            .await?;
        // OCP: set explicitly — default trim=11 gives Imax=100 mA which covers
        // ≤17 dBm (typical ~28 mA).  High-power (+20 dBm) needs 240 mA (trim=27).
        self.device
            .ocp()
            .write_async(|w| {
                w.set_ocp_on(true);
                w.set_ocp_trim(if high_power { 27 } else { 11 });
            })
            .await?;

        // PaDac: must use modify_async — bits 7:3 are reserved and must remain
        // 0b10000 (reset = 0x84).  A full write_async would zero them, putting
        // the PA in an undefined state and killing TX power.
        // 0x84 (pa_dac=4) = normal PA_BOOST (up to +17 dBm).
        // 0x87 (pa_dac=7) = +20 dBm high-power mode on PA_BOOST.
        self.device
            .pa_dac()
            .modify_async(|w| w.set_pa_dac(if high_power { 7 } else { 4 }))
            .await?;

        // FIFO threshold: assert FifoLevel when FIFO contains >= 16 bytes,
        // giving ~48 bytes of headroom before overflow (64-byte FIFO).
        self.device
            .fifo_thresh()
            .write_async(|w| {
                w.set_tx_start_condition(true); // start TX as soon as any byte is in FIFO
                w.set_fifo_threshold(15);
            })
            .await?;

        // Max payload length for variable-length mode (255 for full range).
        self.device
            .payload_length()
            .write_async(|w| w.set_value(255))
            .await?;

        // Preamble length: 16 bytes (matches RadioLib default).
        // POR default is only 3 bytes — marginal with a 2-byte preamble detector.
        self.device
            .preamble_msb()
            .write_async(|w| w.set_value(0))
            .await?;
        self.device
            .preamble_lsb()
            .write_async(|w| w.set_value(16))
            .await?;

        // DIO0 = PacketSent in TX mode (mapping 00, Table 29).
        // Used to wake the executor when the packet has been fully transmitted.
        self.device
            .dio_mapping_1()
            .modify_async(|w| w.set_dio_0_mapping(0)) // 00 = PacketSent
            .await?;

        // Return to Standby and wait for crystal to stabilise (TS_OSC ≈ 250 µs).
        #[cfg(feature = "defmt")]
        debug!("sx127x: entering standby");
        self.device
            .op_mode()
            .modify_async(|w| w.set_mode(Mode::Standby))
            .await?;
        self.wait_mode_ready().await?;

        // Readback critical registers for diagnostics.
        #[cfg(feature = "defmt")]
        {
            let pa = self.device.pa_config().read_async().await?;
            let pa_dac = self.device.pa_dac().read_async().await?;
            let ocp = self.device.ocp().read_async().await?;
            debug!(
                "sx127x: standby ready — pa_select={} output_power={} max_power={} pa_dac={} ocp_trim={}",
                pa.pa_select(),
                pa.output_power(),
                pa.max_power(),
                pa_dac.pa_dac(),
                ocp.ocp_trim(),
            );
        }

        Ok(())
    }

    /// Transmit a framed variable-length packet over GFSK (up to 255 bytes).
    ///
    /// `dio0` must be connected to the radio's DIO0 pin, mapped to PacketSent
    /// in FSK TX mode (mapping 00).  Prefixes the payload with a length byte
    /// and appends a CRC.  Handles FIFO refilling for payloads larger than the
    /// 64-byte hardware FIFO by polling IrqFlags2.FifoLevel via SPI.
    pub async fn transmit<IRQ>(
        &mut self,
        dio0: &mut IRQ,
        data: &[u8],
    ) -> Result<(), Error<S::Error>>
    where
        IRQ: embedded_hal_async::digital::Wait,
    {
        if data.is_empty() || data.len() > 255 {
            return Err(Error::InvalidPacket);
        }

        // Clear stale IRQ flags before staging TX (matches RadioLib stageMode(TX)).
        self.clear_irq_flags().await?;

        // Write length byte + first data chunk in a single SPI transaction.
        let first_chunk = data.len().min(63);
        let mut prefill = [0u8; 64];
        prefill[0] = data.len() as u8;
        prefill[1..1 + first_chunk].copy_from_slice(&data[..first_chunk]);
        self.device
            .fifo()
            .write_async(&prefill[..1 + first_chunk])
            .await?;
        let mut sent = first_chunk;

        // Start transmitting; wait for PLL lock (TS_FS ≈ 60 µs).
        self.device
            .op_mode()
            .modify_async(|w| w.set_mode(Mode::Tx))
            .await?;
        self.wait_mode_ready().await?;

        // Refill FIFO as it drains.  FifoLevel is polled via SPI every 1 ms;
        // at 100 kbps the 64-byte FIFO drains in ~5 ms so 1 ms gives plenty
        // of headroom.
        while sent < data.len() {
            embassy_time::Timer::after_millis(1).await;

            // Check PacketSent first to avoid writing after TX ends.
            let flags = self.device.irq_flags_2().read_async().await?;
            if flags.packet_sent() {
                break;
            }
            if !flags.fifo_level() {
                // FIFO below threshold — refill.
                let chunk = (data.len() - sent).min(48);
                self.device
                    .fifo()
                    .write_async(&data[sent..sent + chunk])
                    .await?;
                sent += chunk;
            }
        }

        // Wait for PacketSent via DIO0 interrupt.  At 100 kbps the last
        // ≤15-byte FIFO drain takes ≤1.2 ms.  Timeout after 100 ms to guard
        // against hardware faults (max 255-byte packet ≈ 22 ms at 100 kbps).
        let _ = embassy_time::with_timeout(
            embassy_time::Duration::from_millis(100),
            dio0.wait_for_rising_edge(),
        )
        .await;

        // Matches RadioLib finishTransmit(): delay → clearIrqFlags → standby.
        // The 1ms delay ensures the PA has fully settled after the last modulated
        // bit before writing FIFO_OVERRUN=1 (which can corrupt it).
        // Standby is entered last so the PA is not killed prematurely.
        embassy_time::Timer::after_millis(1).await;
        self.clear_irq_flags().await?;
        self.device
            .op_mode()
            .modify_async(|w| w.set_mode(Mode::Standby))
            .await?;

        Ok(())
    }
}

// ── GFSK RX ──────────────────────────────────────────────────────────────────

/// Configuration for GFSK receive mode.
pub struct GfskRxConfig {
    /// Carrier frequency in Hz — must match transmitter.
    pub frequency_hz: u32,
    /// Air bitrate in bps — must match transmitter.
    pub bitrate_bps: u32,
    /// 4-byte sync word — must match `GfskConfig::sync_word` exactly.
    pub sync_word: [u8; 4],
}

impl<S: SpiDevice> Sx127x<S> {
    /// Configure the radio for GFSK receive and enter continuous Rx mode.
    ///
    /// After this call, use [`receive`](Self::receive) to collect packets.
    /// The radio stays in Rx mode between packets (auto-restart enabled).
    /// Read the chip version register (always `0x12` on genuine SX127x).
    pub async fn read_version(&mut self) -> Result<u8, Error<S::Error>> {
        Ok(self.device.version().read_async().await?.value() as u8)
    }

    /// Read the current RSSI in dBm (valid in Rx mode).
    pub async fn read_rssi_dbm(&mut self) -> Result<i32, Error<S::Error>> {
        Ok(-(self.device.rssi_value().read_async().await?.value() as i32) / 2)
    }

    /// Read IrqFlags1 diagnostic state: (mode_ready, rx_ready, rssi_triggered, preamble_detected).
    ///
    /// Useful for diagnosing reception failures — if `rssi_triggered` is true but
    /// `preamble_detected` is false, the preamble pattern is not matching.
    pub async fn read_irq_flags_1(&mut self) -> Result<(bool, bool, bool, bool), Error<S::Error>> {
        let f = self.device.irq_flags_1().read_async().await?;
        Ok((f.mode_ready(), f.rx_ready(), f.rssi(), f.preamble_detect()))
    }

    /// Read the current AFC correction in Hz (2's complement, positive = RX crystal runs high).
    pub async fn read_afc_hz(&mut self) -> Result<i32, Error<S::Error>> {
        let msb = self.device.afc_msb().read_async().await?.value() as u16;
        let lsb = self.device.afc_lsb().read_async().await?.value() as u16;
        // Fstep = Fxosc / 2^19 ≈ 61.035 Hz
        let raw = ((msb << 8) | lsb) as i16; // 2's complement
        Ok((raw as i32 * 32_000_000i32) >> 19)
    }

    pub async fn configure_gfsk_rx(&mut self, cfg: &GfskRxConfig) -> Result<(), Error<S::Error>> {
        // Two-step Sleep entry to handle the case where the radio is stuck in
        // LoRa mode from a previous session (radio is not power-cycled on MCU
        // reset).  LongRangeMode can only be changed while already in Sleep.
        //
        // Write 1: transitions Mode → Sleep while preserving LongRangeMode.
        //          If chip was in LoRa non-Sleep, this enters LoRa Sleep.
        // Write 2: now guaranteed to be in Sleep, so LongRangeMode=0 takes
        //          effect and the chip switches to FSK/OOK Sleep.
        self.device
            .op_mode()
            .write_async(|w| {
                w.set_modulation_type(ModulationType::FskOok);
                w.set_low_frequency_mode_on(false);
                w.set_mode(Mode::Sleep);
            })
            .await?;
        self.device
            .op_mode()
            .write_async(|w| {
                w.set_modulation_type(ModulationType::FskOok);
                w.set_low_frequency_mode_on(false);
                w.set_mode(Mode::Sleep);
            })
            .await?;

        // Clear stale IRQ flags now that we're in FSK Sleep (writing flags in
        // LoRa mode targets wrong registers).
        self.clear_irq_flags().await?;

        // Carrier frequency.
        let frf = ((cfg.frequency_hz as u64) << 19) / FXOSC as u64;
        self.device
            .frf()
            .write_async(|w| w.set_value(frf as u32))
            .await?;

        // Bitrate (integer + fractional).
        let bitrate_full = (FXOSC as u64 * 16) / cfg.bitrate_bps as u64;
        let bitrate_int = (bitrate_full / 16) as u16;
        let bitrate_frac = (bitrate_full % 16) as u8;
        self.device
            .bitrate()
            .write_async(|w| w.set_value(bitrate_int))
            .await?;
        self.device
            .bit_rate_frac()
            .write_async(|w| w.set_value(bitrate_frac))
            .await?;

        // RxBw: 200 kHz (Mant20, Exp=1 → Fxosc/(20×2^3) = 200 kHz).
        self.device
            .rx_bw()
            .write_async(|w| {
                w.set_rx_bw_mant(RxBwMant::Mant20);
                w.set_rx_bw_exp(1);
            })
            .await?;

        // AfcBw: 400 kHz (Mant20, Exp=0 → Fxosc/(20×2^2) = 400 kHz).
        self.device
            .afc_bw()
            .write_async(|w| {
                w.set_rx_bw_mant_afc(1); // Mant20
                w.set_rx_bw_exp_afc(0);
            })
            .await?;

        // LNA: maximum gain + HF boost for 915 MHz.
        self.device
            .lna()
            .write_async(|w| {
                w.set_lna_gain(LnaGain::G1);
                w.set_lna_boost_hf(RfiHf::BoostOn);
            })
            .await?;

        // AFC: clear the correction register before each Rx sequence so stale
        // corrections from previous (failed) attempts do not accumulate and
        // pull the demodulator off-frequency.
        self.device
            .afc_fei()
            .write_async(|w| {
                w.set_afc_auto_clear_on(true); // reset AFC offset at start of each Rx
            })
            .await?;

        // RxConfig: AFC auto + AGC auto on.
        // rx_trigger=6 (PreambleDetect): bit-clock recovery starts precisely
        // when preamble bytes are confirmed, giving the tightest phase lock
        // before sync-word detection.  This is the POR default and is more
        // reliable than rx_trigger=1 (RSSI), which starts recovery the moment
        // RSSI threshold is crossed (potentially mid-noise).
        self.device
            .rx_config()
            .write_async(|w| {
                w.set_afc_auto_on(true);
                w.set_agc_auto_on(true);
                w.set_rx_trigger(6); // PreambleDetect interrupt
            })
            .await?;

        // Preamble detector: on, 2 bytes, 10 chip tolerance.
        self.device
            .preamble_detect()
            .write_async(|w| {
                w.set_preamble_detector_on(true);
                w.set_preamble_detector_size(PreambleDetectorSize::TwoBytes);
                w.set_preamble_detector_tol(10);
            })
            .await?;

        // Sync word: 4 bytes from config (must match TX exactly).
        self.device
            .sync_config()
            .write_async(|w| {
                w.set_auto_restart_rx_mode(AutoRestartRxMode::OnWithoutPllRelock);
                w.set_sync_on(true);
                w.set_sync_size(3); // 4 bytes
            })
            .await?;
        // SyncValue registers: set explicitly from config to match TX.
        self.device
            .sync_value_1()
            .write_async(|w| w.set_value(cfg.sync_word[0]))
            .await?;
        self.device
            .sync_value_2()
            .write_async(|w| w.set_value(cfg.sync_word[1]))
            .await?;
        self.device
            .sync_value_3()
            .write_async(|w| w.set_value(cfg.sync_word[2]))
            .await?;
        self.device
            .sync_value_4()
            .write_async(|w| w.set_value(cfg.sync_word[3]))
            .await?;

        // Packet config: variable length, CRC on, packet mode.
        self.device
            .packet_config_1()
            .write_async(|w| {
                w.set_packet_format(true); // variable length
                w.set_crc_on(true);
                w.set_crc_auto_clear_off(true); // we check crc_ok ourselves
            })
            .await?;
        self.device
            .packet_config_2()
            .write_async(|w| w.set_data_mode(true))
            .await?;

        // Max payload: 255 bytes (variable mode upper bound).
        self.device
            .payload_length()
            .write_async(|w| w.set_value(255))
            .await?;

        // FIFO threshold: FifoLevel fires when FIFO has > 15 bytes.
        // Gives us time to drain before overflow at 100 kbps.
        self.device
            .fifo_thresh()
            .write_async(|w| w.set_fifo_threshold(15))
            .await?;

        // DIO0 = PayloadReady in FSK packet mode (mapping 0b00 is the default but be explicit).
        self.device
            .dio_mapping_1()
            .modify_async(|w| w.set_dio_0_mapping(0))
            .await?;

        // Enter Standby first so the oscillator and PLL can stabilise
        // (mirrors configure_gfsk_tx).  Going directly from Sleep to Rx races
        // the PLL lock and causes wait_mode_ready() to time out.
        self.device
            .op_mode()
            .modify_async(|w| w.set_mode(Mode::Standby))
            .await?;
        self.wait_mode_ready().await?;

        // Enter Rx mode.  Poll PllLock (not ModeReady) — some modules assert
        // PllLock but never assert ModeReady in Rx mode.  Once PLL is locked
        // (≈60 µs) the receiver chain is active; a 2 ms settle covers RSSI
        // and AGC stabilisation before the first packet can arrive.
        #[cfg(feature = "defmt")]
        debug!("sx127x: entering rx");
        self.device
            .op_mode()
            .modify_async(|w| w.set_mode(Mode::Rx))
            .await?;

        // Wait up to 10 ms for PLL lock, then allow 2 ms for RSSI/AGC settle.
        let mut polls = 0u32;
        loop {
            let flags = self.device.irq_flags_1().read_async().await?;
            if flags.pll_lock() {
                break;
            }
            polls += 1;
            if polls >= 1_000 {
                return Err(Error::Timeout);
            }
        }
        embassy_time::Timer::after_millis(2).await;

        #[cfg(feature = "defmt")]
        defmt::debug!("sx127x: rx ready");

        Ok(())
    }

    /// Receive one variable-length GFSK packet (up to 255 bytes).
    ///
    /// `irq` must be connected to the radio's DIO0 pin, which is mapped to
    /// PayloadReady in FSK packet mode.  Returns the number of payload bytes
    /// written into `buf`.
    ///
    /// For packets larger than the 66-byte hardware FIFO (e.g. 160-byte Opus
    /// frames at 64 kbps) the FIFO is drained via a 1 ms timer loop while
    /// DIO0 acts as the final completion signal, eliminating the tight SPI
    /// polling loop that previously saturated the DMA interrupt system.
    pub async fn receive<IRQ>(
        &mut self,
        irq: &mut IRQ,
        buf: &mut [u8],
    ) -> Result<usize, Error<S::Error>>
    where
        IRQ: embedded_hal_async::digital::Wait,
    {
        #[cfg(feature = "defmt")]
        defmt::assert!(
            buf.len() >= 255,
            "receive buffer must be at least 255 bytes"
        );

        // Wait for the first bytes to arrive (FifoLevel ≥16 bytes for large
        // packets, or PayloadReady for short packets that fit in the FIFO
        // entirely).  Poll every 1 ms — no tight DMA loop.
        // Times out after 1 s so callers can log diagnostics if stuck.
        const OUTER_TIMEOUT_MS: u32 = 1_000;
        let mut waited_ms = 0u32;
        loop {
            embassy_time::Timer::after_millis(1).await;
            let flags = self.device.irq_flags_2().read_async().await?;
            if flags.fifo_level() || flags.payload_ready() {
                break;
            }
            waited_ms += 1;
            if waited_ms >= OUTER_TIMEOUT_MS {
                #[cfg(feature = "defmt")]
                {
                    let f1 = self.device.irq_flags_1().read_async().await?;
                    let f2 = self.device.irq_flags_2().read_async().await?;
                    let afc_hz = self.read_afc_hz().await.unwrap_or(i32::MIN);
                    debug!(
                        "sx127x: rx timeout — f1: mode_ready={} rx_ready={} rssi={} preamble={} sync_match={} | f2: fifo_empty={} fifo_level={} payload_ready={} crc_ok={} | afc={}Hz",
                        f1.mode_ready(),
                        f1.rx_ready(),
                        f1.rssi(),
                        f1.preamble_detect(),
                        f1.sync_address_match(),
                        f2.fifo_empty(),
                        f2.fifo_level(),
                        f2.payload_ready(),
                        f2.crc_ok(),
                        afc_hz
                    );
                }
                // Flush the FIFO (clears any partial packet data) and
                // re-enter RX so the radio is ready for the next packet.
                self.device
                    .irq_flags_2()
                    .write_async(|w| w.set_fifo_overrun(true))
                    .await?;
                self.clear_irq_flags().await?;
                self.device
                    .op_mode()
                    .modify_async(|w| w.set_mode(Mode::Rx))
                    .await?;
                return Err(Error::Timeout);
            }
        }

        // First byte is always the length field in variable-length mode.
        let mut len_buf = [0u8; 1];
        self.device.fifo().read_async(&mut len_buf).await?;
        let len = len_buf[0] as usize;
        #[cfg(feature = "defmt")]
        debug!("sx127x: activity detected, len byte={}", len);

        if len == 0 || len > buf.len() {
            // Invalid length — clear the FIFO so the radio can auto-restart.
            // (Auto-restart only fires after FIFO is empty; leaving bytes
            // would stall reception indefinitely.)
            self.device
                .irq_flags_2()
                .write_async(|w| w.set_fifo_overrun(true))
                .await?;
            return Err(Error::InvalidPacket);
        }

        // Drain the FIFO while the packet is being received.
        //
        // DIO0 (PayloadReady) fires via GPIO interrupt — much more responsive
        // than SPI polling.  Between interrupts, drain FifoLevel chunks every
        // 1 ms so the 64-byte FIFO never overflows at 100 kbps.
        //
        // wait_for_high is used (not wait_for_rising_edge) because for short
        // packets PayloadReady may already be asserted before we enter this
        // loop.  Timeout after 500 ms guards against TX dying mid-packet.
        let mut received = 0;
        let mut drain_ms = 0u32;
        const DRAIN_TIMEOUT_MS: u32 = 500;
        let result = loop {
            match embassy_time::with_timeout(
                embassy_time::Duration::from_millis(1),
                irq.wait_for_high(),
            )
            .await
            {
                Ok(_) => {
                    // PayloadReady: latch crc_ok before auto-restart clears it,
                    // then read all remaining bytes from the FIFO.
                    let flags = self.device.irq_flags_2().read_async().await?;
                    let crc_ok = flags.crc_ok();
                    let remaining = len - received;
                    if remaining > 0 {
                        self.device
                            .fifo()
                            .read_async(&mut buf[received..len])
                            .await?;
                    }
                    if !crc_ok {
                        break Err(Error::CrcError);
                    }
                    break Ok(len);
                }
                Err(_timeout) => {
                    drain_ms += 1;
                    if drain_ms >= DRAIN_TIMEOUT_MS {
                        break Err(Error::Timeout);
                    }
                    // 1 ms elapsed — drain FIFO if threshold is reached.
                    let flags = self.device.irq_flags_2().read_async().await?;
                    if flags.fifo_level() {
                        let remaining = len.saturating_sub(received);
                        if remaining == 0 {
                            break Ok(len);
                        }
                        let chunk = remaining.min(16);
                        self.device
                            .fifo()
                            .read_async(&mut buf[received..received + chunk])
                            .await?;
                        received += chunk;
                    }
                }
            }
        };

        // On any error, flush the FIFO and re-enter RX so the radio is
        // ready for the next packet.  Without this, a partial packet or
        // CRC failure leaves stale bytes that prevent auto-restart.
        if result.is_err() {
            self.device
                .irq_flags_2()
                .write_async(|w| w.set_fifo_overrun(true))
                .await?;
            self.clear_irq_flags().await?;
            self.device
                .op_mode()
                .modify_async(|w| w.set_mode(Mode::Rx))
                .await?;
        }

        result
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Compute PA pin, output_power register, max_power, and whether PaDac
/// high-power mode is needed.
///
/// Returns `(pa_select, output_power, max_power, high_power)`.
fn gfsk_pa_settings(tx_power_dbm: i8) -> (PaSelect, u8, u8, bool) {
    if tx_power_dbm <= 13 {
        // RFM95W: RFO_HF is NOT routed to the module antenna pin — PA_BOOST
        // is the only usable PA.  Use PA_BOOST for all power levels.
        // PA_BOOST: Pout = 2 + OutputPower (range 2..=17 dBm).
        // Clamp tx_power_dbm to minimum 2 (OutputPower=0, Pout=2 dBm).
        let output_power = (tx_power_dbm.clamp(2, 13) - 2) as u8;
        (PaSelect::PaBoost, output_power, 4, false)
    } else if tx_power_dbm <= 17 {
        // PA_BOOST: Pout = 2 + OutputPower
        let output_power = (tx_power_dbm - 2) as u8;
        (PaSelect::PaBoost, output_power, 4, false)
    } else {
        // PA_BOOST + PaDac=0x07: unlocks +20 dBm, requires OutputPower = 15.
        (PaSelect::PaBoost, 15, 4, true)
    }
}
