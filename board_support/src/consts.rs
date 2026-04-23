pub const SAMPLE_RATE: u32 = 48_000;
pub const I2S_CLK_HZ: u32 = 12_288_000;
pub const RADIO_FREQ_HZ: u32 = 915_000_000;
/// Air bitrate shared between TX and RX — must match on both sides.
pub const RADIO_BITRATE_BPS: u32 = 100_000;
/// Peak frequency deviation — must match on both sides.
pub const RADIO_FDEV_HZ: u32 = 50_000;
/// 4-byte sync word — must be identical on TX and RX.
/// 0x2D/0xD4 alternation: each byte has exactly 4 bits set (50% density) and
/// the pair is bitwise complementary, giving good autocorrelation and low
/// false-trigger probability on a quiet channel.
pub const RADIO_SYNC_WORD: [u8; 4] = [0x2D, 0xD4, 0x2D, 0xD4];
/// 20ms stereo frame at SAMPLE_RATE: 960 samples × 2 channels = 1920 i16.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE / 1000 * 20) as usize * 2;
/// Hard per-frame cap on Opus output so every packet fits a single radio TX.
/// 100 kbps × 20 ms = 250 bytes on-air minus ~15 bytes framing overhead
/// (8 preamble + 4 sync + 1 length + 2 CRC).
pub const OPUS_BUF_SIZE: usize = 235;

/// Stereo interleaved i16 PCM, 20ms at 48 kHz.
pub type AudioFrame = [i16; FRAME_SAMPLES];
/// Packed stereo u32 data out of the I2S DMA engine.
/// Each u32 = (left << 16) | right (from PIO ShiftLeft autopush).
/// On little-endian ARM, reinterpreted as i16 via bytemuck this gives
/// [right, left] per sample — channels swapped from convention, but
/// consistent through the encode→decode round-trip.
/// Kept as u32 to guarantee 4-byte alignment required by DMA.
pub type PackedAudioFrame = [u32; FRAME_SAMPLES / 2];

/// Opus-encoded packet with its length.
#[derive(Clone, Copy)]
pub struct OpusPacket {
    pub data: [u8; OPUS_BUF_SIZE],
    pub len: usize,
}
