use crate::error::Error;
use device_driver::{
    AsyncBufferInterface, AsyncRegisterInterface, BufferInterfaceError, create_device,
};
use embedded_hal_async::spi::{Operation, SpiDevice};

create_device!(
    device_name: Device,
    dsl: {
        config {
            type RegisterAddressType = u8;
            type BufferAddressType = u8;
            type DefmtFeature = "defmt";
            type DefaultByteOrder = BE;
        }
        // FIFO
        buffer Fifo: RW = 0x00,
        // Registers for common settings
        register OpMode {
            const ADDRESS = 0x01;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0b0000101;
            long_range_mode: RO uint as enum LongRangeMode {
                FskOokMode = 0,
                LoRaMode = 1,
            } = 7..=7,
            modulation_type: uint as enum ModulationType {
                FskOok = 0,
                LoRa = 1,
                _reserved = catch_all,
            } = 5..=6,
            low_frequency_mode_on: bool = 3,
            mode: uint as enum Mode {
                Sleep,
                Standby,
                FSTx,
                Tx,
                FSRx,
                Rx,
                _reserved = catch_all,
            } = 0..=2,
        },
        register Bitrate {
            const ADDRESS = 0x02;
            const SIZE_BITS = 16;
            const RESET_VALUE = 0x1A0B;
            value: uint = 0..=15,
        },
        register Fdev {
            const ADDRESS = 0x04;
            const SIZE_BITS = 16;
            const RESET_VALUE = 0x0052;
            value: uint = 0..=13,
        },
        register Frf {
            const ADDRESS = 0x06;
            const SIZE_BITS = 24;
            const RESET_VALUE = 0x6C8000;
            value: uint = 0..=23,
        },
        // Registers for the transmitter
        register PaConfig {
            const ADDRESS = 0x09;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0b01001111;
            pa_select: uint as enum PaSelect {
                /// Maximum power of +14 dBm
                Rfo = 0,
                /// Maximum power of +20 dBm
                PaBoost,
            } = 7..=7,
            /// Pmax = 10.8 + 0.6 * MaxPower (dBm)
            max_power: uint = 4..=6,
            /// If pa_select = RFO, Pmax - (15-OutputPower), else 17 - (15 - OutputPower)
            output_power: uint = 0..=3,
        },
        register PaRampModShape {
            const ADDRESS = 0x0A;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x09;
            modulation_shaping: uint as enum ModulationShaping {
                NoShaping,
                /// Gaussian BT=1.0; Fcutoff = bit_rate in OOK
                GaussianBT1_0,
                /// Gaussian BT=0.5; Fcutoff = 2*bit_rate for bit_rate < 125 kb/s in OOK
                GaussianBT0_5,
                /// Gaussian BT=0.3; reserved in OOK
                GaussianBT0_3,
            } = 5..=6,
            pa_ramp: uint as enum PaRamp {
                Ramp3_4ms,
                Ramp2ms,
                Ramp1ms,
                Ramp500us,
                Ramp250us,
                Ramp125us,
                Ramp100us,
                Ramp62us,
                Ramp50us,
                Ramp40us = default,
                Ramp31us,
                Ramp25us,
                Ramp20us,
                Ramp15us,
                Ramp12us,
                Ramp10us,
            } = 0..=3,
        },
        register Ocp {
            const ADDRESS = 0x0B;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0b00101011;
            ocp_on: bool = 5,
            ocp_trim: uint = 0..=4,
        },
        // Registers for the receiver
        register Lna {
            const ADDRESS = 0x0C;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0b00100000;
            lna_gain: uint as enum LnaGain {
                /// Highest gain
                G1 = 1,
                /// Highest gain - 6 dB
                G2,
                /// Highest gain - 12 dB
                G3,
                /// Highest gain - 24 dB
                G4,
                /// Highest gain - 36 dB
                G5,
                /// Highest gain - 48 dB
                G6,
                _reserved = catch_all,
            } = 5..=7,
            lna_boost_hf: uint as enum RfiHf {
                DefaultLnaCurrent,
                _reserved = catch_all,
                BoostOn = 0b11,
            } = 0..=1,
        },
        register RxConfig {
            const ADDRESS = 0x0D;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0b00001110;
            restart_rx_on_collision: bool = 7,
            /// Write 1 to manually restart Rx (no frequency change)
            restart_rx_without_pll_lock: bool = 6,
            /// Write 1 to manually restart Rx (frequency change, waits for PLL)
            restart_rx_with_pll_lock: bool = 5,
            afc_auto_on: bool = 4,
            agc_auto_on: bool = 3,
            /// See Table 24 in datasheet
            rx_trigger: uint = 0..=2,
        },
        register RssiConfig {
            const ADDRESS = 0x0E;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0b00000010;
            rssi_offset: int = 3..=7,
            rssi_smoothing: uint as enum RssiSmoothing {
                Smooth2,
                Smooth4,
                Smooth8,
                Smooth16,
                Smooth32,
                Smooth64,
                Smooth128,
                Smooth256,
            } = 0..=2,
        },
        register RssiCollision {
            const ADDRESS = 0x0F;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x0A;
            value: uint = 0..=7,
        },
        register RssiThresh {
            const ADDRESS = 0x10;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0xFF;
            value: uint = 0..=7,
        },
        register RssiValue {
            const ADDRESS = 0x11;
            const SIZE_BITS = 8;
            value: RO uint = 0..=7,
        },
        register RxBw {
            const ADDRESS = 0x12;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0b00010110;
            rx_bw_mant: uint as enum RxBwMant {
                Mant16,
                Mant20,
                Mant24,
                _reserved = catch_all,
            } = 3..=4,
            rx_bw_exp: uint = 0..=2,
        },
        register AfcBw {
            const ADDRESS = 0x13;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0b00001011;
            rx_bw_mant_afc: uint = 3..=4,
            rx_bw_exp_afc: uint = 0..=2,
        },
        register OokPeak {
            const ADDRESS = 0x14;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x28;
            bit_sync_on: bool = 5,
            ook_thresh_type: uint as enum OokThreshType {
                Fixed,
                Peak,
                Average,
                _reserved = catch_all,
            } = 3..=4,
            ook_peak_thresh_step: uint as enum OokPeakThreshStep {
                /// 0.5 dB
                Step0_5dB,
                /// 1.0 dB
                Step1_0dB,
                /// 1.5 dB
                Step1_5dB,
                /// 2.0 dB
                Step2_0dB,
                /// 3.0 dB
                Step3_0dB,
                /// 4.0 dB
                Step4_0dB,
                /// 5.0 dB
                Step5_0dB,
                /// 6.0 dB
                Step6_0dB,
            } = 0..=2,
        },
        register OokFix {
            const ADDRESS = 0x15;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x0C;
            /// Fixed threshold for OOK data slicer / floor threshold in peak mode
            value: uint = 0..=7,
        },
        register OokAvg {
            const ADDRESS = 0x16;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x12;
            ook_peak_thresh_dec: uint as enum OokPeakThreshDec {
                OncePerChip,
                OnceEvery2Chips,
                OnceEvery4Chips,
                OnceEvery8Chips,
                TwicePerChip,
                FourTimesPerChip,
                EightTimesPerChip,
                SixteenTimesPerChip,
            } = 5..=7,
            ook_average_offset: uint as enum OokAverageOffset {
                /// 0.0 dB
                Offset0_0dB,
                /// 2.0 dB
                Offset2_0dB,
                /// 4.0 dB
                Offset4_0dB,
                /// 6.0 dB
                Offset6_0dB,
            } = 2..=3,
            ook_average_thresh_filt: uint as enum OokAverageThreshFilt {
                /// fc ≈ chip rate / 32π
                ChipRateDiv32Pi,
                /// fc ≈ chip rate / 8π
                ChipRateDiv8Pi,
                /// fc ≈ chip rate / 4π
                ChipRateDiv4Pi,
                /// fc ≈ chip rate / 2π
                ChipRateDiv2Pi,
            } = 0..=1,
        },
        // 0x17-0x19: reserved
        register AfcFei {
            const ADDRESS = 0x1A;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// Write 1 to trigger AGC sequence
            agc_start: bool = 4,
            /// Clear AFC register in Rx mode. Always reads 0.
            afc_clear: bool = 1,
            afc_auto_clear_on: bool = 0,
        },
        register AfcMsb {
            const ADDRESS = 0x1B;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// MSB of AFC value (2's complement). Can overwrite current AFC.
            value: uint = 0..=7,
        },
        register AfcLsb {
            const ADDRESS = 0x1C;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// LSB of AFC value (2's complement)
            value: uint = 0..=7,
        },
        register FeiMsb {
            const ADDRESS = 0x1D;
            const SIZE_BITS = 8;
            /// MSB of measured frequency offset (2's complement). Read before FeiLsb.
            value: RO uint = 0..=7,
        },
        register FeiLsb {
            const ADDRESS = 0x1E;
            const SIZE_BITS = 8;
            /// LSB of measured frequency offset. Freq error = FeiValue * Fstep.
            value: RO uint = 0..=7,
        },
        register PreambleDetect {
            const ADDRESS = 0x1F;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x40;
            preamble_detector_on: bool = 7,
            preamble_detector_size: uint as enum PreambleDetectorSize {
                OneByte,
                TwoBytes,
                ThreeBytes,
                _reserved = catch_all,
            } = 5..=6,
            /// Number of chip errors tolerated over PreambleDetectorSize. 4 chips per bit.
            preamble_detector_tol: uint = 0..=4,
        },
        register RxTimeout1 {
            const ADDRESS = 0x20;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// Timeout after value*16*Tbit if no RSSI interrupt. 0 = disabled.
            value: uint = 0..=7,
        },
        register RxTimeout2 {
            const ADDRESS = 0x21;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// Timeout after value*16*Tbit if no preamble detected. 0 = disabled.
            value: uint = 0..=7,
        },
        register RxTimeout3 {
            const ADDRESS = 0x22;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// Timeout after value*16*Tbit if no sync address. 0 = disabled.
            value: uint = 0..=7,
        },
        register RxDelay {
            const ADDRESS = 0x23;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// Additional delay before auto receiver restart. Delay = value * 4 * Tbit.
            value: uint = 0..=7,
        },
        register Osc {
            const ADDRESS = 0x24;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x05;
            /// Write 1 to trigger RC oscillator calibration (Standby mode only). Always reads 0.
            rc_cal_start: bool = 3,
            clk_out: uint as enum ClkOut {
                Fxosc,
                FxoscDiv2,
                FxoscDiv4,
                FxoscDiv8,
                FxoscDiv16,
                FxoscDiv32,
                /// RC oscillator (automatically enabled)
                Rc,
                Off,
            } = 0..=2,
        },
        register PreambleMsb {
            const ADDRESS = 0x25;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            value: uint = 0..=7,
        },
        register PreambleLsb {
            const ADDRESS = 0x26;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x03;
            value: uint = 0..=7,
        },
        register SyncConfig {
            const ADDRESS = 0x27;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x93;
            auto_restart_rx_mode: uint as enum AutoRestartRxMode {
                Off,
                OnWithoutPllRelock,
                OnWithPllRelock,
                _reserved = catch_all,
            } = 6..=7,
            /// 0 = 0xAA preamble polarity, 1 = 0x55
            preamble_polarity: bool = 5,
            sync_on: bool = 4,
            /// Sync word size = sync_size + 1 bytes (sync_size bytes if io_home_on = 1)
            sync_size: uint = 0..=2,
        },
        register SyncValue1 {
            const ADDRESS = 0x28;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x01;
            value: uint = 0..=7,
        },
        register SyncValue2 {
            const ADDRESS = 0x29;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x01;
            value: uint = 0..=7,
        },
        register SyncValue3 {
            const ADDRESS = 0x2A;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x01;
            value: uint = 0..=7,
        },
        register SyncValue4 {
            const ADDRESS = 0x2B;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x01;
            value: uint = 0..=7,
        },
        register SyncValue5 {
            const ADDRESS = 0x2C;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x01;
            value: uint = 0..=7,
        },
        register SyncValue6 {
            const ADDRESS = 0x2D;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x01;
            value: uint = 0..=7,
        },
        register SyncValue7 {
            const ADDRESS = 0x2E;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x01;
            value: uint = 0..=7,
        },
        register SyncValue8 {
            const ADDRESS = 0x2F;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x01;
            value: uint = 0..=7,
        },
        register PacketConfig1 {
            const ADDRESS = 0x30;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x90;
            /// 0 = fixed length, 1 = variable length
            packet_format: bool = 7,
            dc_free: uint as enum DcFree {
                None,
                Manchester,
                Whitening,
                _reserved = catch_all,
            } = 5..=6,
            crc_on: bool = 4,
            /// 0 = clear FIFO and restart on CRC fail, 1 = do not clear FIFO
            crc_auto_clear_off: bool = 3,
            address_filtering: uint as enum AddressFiltering {
                None,
                NodeAddress,
                NodeOrBroadcast,
                _reserved = catch_all,
            } = 1..=2,
            /// 0 = CCITT CRC + standard whitening, 1 = IBM CRC + alternate whitening
            crc_whitening_type: bool = 0,
        },
        register PacketConfig2 {
            const ADDRESS = 0x31;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x40;
            /// 0 = continuous mode, 1 = packet mode
            data_mode: bool = 6,
            io_home_on: bool = 5,
            io_home_power_frame: bool = 4,
            beacon_on: bool = 3,
            /// Packet length MSBits [10:8]
            payload_length_msb: uint = 0..=2,
        },
        register PayloadLength {
            const ADDRESS = 0x32;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x40;
            /// Fixed mode: payload length. Variable mode: max Rx length.
            value: uint = 0..=7,
        },
        register NodeAdrs {
            const ADDRESS = 0x33;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            value: uint = 0..=7,
        },
        register BroadcastAdrs {
            const ADDRESS = 0x34;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            value: uint = 0..=7,
        },
        register FifoThresh {
            const ADDRESS = 0x35;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x8F;
            /// 0 = start Tx when FIFO exceeds threshold, 1 = start when >= 1 byte in FIFO
            tx_start_condition: bool = 7,
            /// FifoLevel interrupt when bytes in FIFO >= fifo_threshold + 1
            fifo_threshold: uint = 0..=5,
        },
        // Sequencer registers
        register SeqConfig1 {
            const ADDRESS = 0x36;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// Write 1 to start sequencer (Sleep or Standby mode only)
            sequencer_start: bool = 7,
            /// Write 1 to force sequencer off. Always reads 0.
            sequencer_stop: bool = 6,
            /// 0 = Standby, 1 = Sleep idle mode
            idle_mode: bool = 5,
            from_start: uint as enum FromStart {
                ToLowPowerSelection,
                ToReceive,
                ToTransmit,
                ToTransmitOnFifoLevel,
            } = 3..=4,
            low_power_selection: bool = 2,
            from_idle: bool = 1,
            from_transmit: bool = 0,
        },
        register SeqConfig2 {
            const ADDRESS = 0x37;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            from_receive: uint = 5..=7,
            from_rx_timeout: uint as enum FromRxTimeout {
                ToReceiveViaReceiveRestart,
                ToTransmit,
                ToLowPowerSelection,
                ToSequencerOff,
            } = 3..=4,
            from_packet_received: uint = 0..=2,
        },
        register TimerResol {
            const ADDRESS = 0x38;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            timer1_resolution: uint as enum Timer1Resolution {
                Disabled,
                /// 64 us
                Us64,
                /// 4.1 ms
                Ms4_1,
                /// 262 ms
                Ms262,
            } = 2..=3,
            timer2_resolution: uint as enum Timer2Resolution {
                Disabled,
                /// 64 us
                Us64,
                /// 4.1 ms
                Ms4_1,
                /// 262 ms
                Ms262,
            } = 0..=1,
        },
        register Timer1Coef {
            const ADDRESS = 0x39;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0xF5;
            value: uint = 0..=7,
        },
        register Timer2Coef {
            const ADDRESS = 0x3A;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x20;
            value: uint = 0..=7,
        },
        // Service registers
        register ImageCal {
            const ADDRESS = 0x3B;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x02;
            auto_image_cal_on: bool = 7,
            /// Write 1 to trigger IQ and RSSI calibration (Standby mode only)
            image_cal_start: bool = 6,
            image_cal_running: RO bool = 5,
            temp_change: RO bool = 3,
            temp_threshold: uint as enum TempThreshold {
                /// 5 °C
                Deg5,
                /// 10 °C
                Deg10,
                /// 15 °C
                Deg15,
                /// 20 °C
                Deg20,
            } = 1..=2,
            temp_monitor_off: bool = 0,
        },
        register Temp {
            const ADDRESS = 0x3C;
            const SIZE_BITS = 8;
            /// Measured temperature. -1 °C per LSB. Needs calibration for absolute accuracy.
            value: RO int = 0..=7,
        },
        register LowBat {
            const ADDRESS = 0x3D;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x02;
            low_bat_on: bool = 3,
            low_bat_trim: uint as enum LowBatTrim {
                /// 1.695 V
                V1_695,
                /// 1.764 V
                V1_764,
                /// 1.835 V
                V1_835,
                /// 1.905 V (default)
                V1_905 = default,
                /// 1.976 V
                V1_976,
                /// 2.045 V
                V2_045,
                /// 2.116 V
                V2_116,
                /// 2.185 V
                V2_185,
            } = 0..=2,
        },
        // Status registers
        register IrqFlags1 {
            const ADDRESS = 0x3E;
            const SIZE_BITS = 8;
            mode_ready: RO bool = 7,
            rx_ready: RO bool = 6,
            tx_ready: RO bool = 5,
            pll_lock: RO bool = 4,
            /// Set when RssiValue > RssiThreshold. Write 1 to clear.
            rssi: bool = 3,
            timeout: RO bool = 2,
            /// Set when preamble detected. Write 1 to clear.
            preamble_detect: bool = 1,
            /// Set when sync+address detected. Write 1 to clear (Continuous mode only).
            sync_address_match: bool = 0,
        },
        register IrqFlags2 {
            const ADDRESS = 0x3F;
            const SIZE_BITS = 8;
            fifo_full: RO bool = 7,
            fifo_empty: RO bool = 6,
            fifo_level: RO bool = 5,
            /// Set on FIFO overrun. Write 1 to clear and unlock FIFO.
            fifo_overrun: bool = 4,
            packet_sent: RO bool = 3,
            payload_ready: RO bool = 2,
            crc_ok: RO bool = 1,
            /// Set when battery below LowBat threshold. Write 1 to clear.
            low_bat: bool = 0,
        },
        // IO control registers
        register DioMapping1 {
            const ADDRESS = 0x40;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// See Table 29/30 for FSK/OOK mappings
            dio0_mapping: uint = 6..=7,
            dio1_mapping: uint = 4..=5,
            dio2_mapping: uint = 2..=3,
            dio3_mapping: uint = 0..=1,
        },
        register DioMapping2 {
            const ADDRESS = 0x41;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            dio4_mapping: uint = 6..=7,
            dio5_mapping: uint = 4..=5,
            /// 0 = Rssi interrupt, 1 = PreambleDetect interrupt mapped to DIO pins
            map_preamble_detect: bool = 0,
        },
        // Version register
        register Version {
            const ADDRESS = 0x42;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x12;
            /// Full revision [7:4], metal mask revision [3:0]
            value: RO uint = 0..=7,
        },
        // Additional registers
        register PllHop {
            const ADDRESS = 0x44;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x2D;
            /// 1 = Frf validated when RegFrfLsb written, enabling fast frequency hopping
            fast_hop_on: bool = 7,
        },
        register Tcxo {
            const ADDRESS = 0x4B;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x09;
            /// 0 = crystal oscillator, 1 = external clipped sine TCXO on XTA pin
            tcxo_input_on: bool = 4,
        },
        register PaDac {
            const ADDRESS = 0x4D;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x84;
            /// 0x04 = default, 0x07 = +20 dBm on PA_BOOST (requires OutputPower = 0b1111)
            pa_dac: uint = 0..=2,
        },
        register FormerTemp {
            const ADDRESS = 0x5B;
            const SIZE_BITS = 8;
            /// Temperature at last IQ calibration. Same format as Temp register.
            value: RO int = 0..=7,
        },
        register BitRateFrac {
            const ADDRESS = 0x5D;
            const SIZE_BITS = 8;
            const RESET_VALUE = 0x00;
            /// Fractional part of bit rate divider (FSK only).
            /// BitRate = Fxosc / (BitRate(15,0) + BitRateFrac/16)
            value: uint = 0..=3,
        },
    }
);

#[derive(Debug)]
pub struct DeviceInterface<S> {
    spi: S,
}

impl<S> DeviceInterface<S> {
    pub(crate) const fn new(spi: S) -> Self {
        Self { spi }
    }
}

// See page 80 of the datasheet for the SPI implementation details:
// The SPI interface gives access to the configuration register via a synchronous full-duplex protocol corresponding to
// CPOL = 0 and CPHA = 0 in Motorola/Freescale nomenclature.

// SINGLE access: an address byte followed by a data byte is sent for a write access whereas an address byte is sent and
// a read byte is received for the read access. The NSS pin goes low at the beginning of the frame and goes high after the
// data byte.
//
// BURST access: the address byte is followed by several data bytes. The address is automatically incremented internally
// between each data byte. This mode is available for both read and write accesses. The NSS pin goes low at the
// beginning of the frame and stay low between each byte. It goes high only after the last byte transfer.
//
// FIFO access: if the address byte corresponds to the address of the FIFO, then succeeding data byte will address the
// FIFO. The address is not automatically incremented but is memorized and does not need to be sent between each data
// byte. The NSS pin goes low at the beginning of the frame and stay low between each byte. It goes high only after the
// last byte transfer.

impl<S: SpiDevice> AsyncRegisterInterface for DeviceInterface<S> {
    type Error = Error<S::Error>;
    type AddressType = u8;

    async fn write_register(
        &mut self,
        address: Self::AddressType,
        _size_bits: u32,
        data: &[u8],
    ) -> Result<(), Self::Error> {
        self.spi
            .transaction(&mut [Operation::Write(&[0x80 | address]), Operation::Write(data)])
            .await?;
        Ok(())
    }

    async fn read_register(
        &mut self,
        address: Self::AddressType,
        _size_bits: u32,
        data: &mut [u8],
    ) -> Result<(), Self::Error> {
        self.spi
            .transaction(&mut [Operation::Write(&[address]), Operation::Read(data)])
            .await?;
        Ok(())
    }
}

impl<S: SpiDevice> BufferInterfaceError for DeviceInterface<S> {
    type Error = Error<S::Error>;
}

impl<S: SpiDevice> AsyncBufferInterface for DeviceInterface<S> {
    type AddressType = u8;

    async fn write(
        &mut self,
        address: Self::AddressType,
        buf: &[u8],
    ) -> Result<usize, Self::Error> {
        self.spi
            .transaction(&mut [Operation::Write(&[0x80 | address]), Operation::Write(buf)])
            .await?;
        Ok(buf.len())
    }

    async fn flush(&mut self, _address: Self::AddressType) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn read(
        &mut self,
        address: Self::AddressType,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error> {
        self.spi
            .transaction(&mut [Operation::Write(&[address]), Operation::Read(buf)])
            .await?;
        Ok(buf.len())
    }
}
