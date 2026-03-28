#![no_std]

// Opus error codes
pub const OPUS_OK: i32 = 0;
pub const OPUS_BAD_ARG: i32 = -1;
pub const OPUS_BUFFER_TOO_SMALL: i32 = -2;
pub const OPUS_INTERNAL_ERROR: i32 = -3;
pub const OPUS_INVALID_PACKET: i32 = -4;
pub const OPUS_UNIMPLEMENTED: i32 = -5;
pub const OPUS_INVALID_STATE: i32 = -6;
pub const OPUS_ALLOC_FAIL: i32 = -7;

// Encoder CTL request codes
pub const OPUS_SET_BITRATE_REQUEST: i32 = 4002;
pub const OPUS_SET_COMPLEXITY_REQUEST: i32 = 4010;
pub const OPUS_SET_IGNORE_EXTENSIONS_REQUEST: i32 = 4058;

// Sentinel bitrate values
pub const OPUS_AUTO: i32 = -1000;
pub const OPUS_BITRATE_MAX: i32 = -1;

// Opus application modes
pub const OPUS_APPLICATION_VOIP: i32 = 2048;
pub const OPUS_APPLICATION_AUDIO: i32 = 2049;
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: i32 = 2051;

#[repr(C)]
pub struct OpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
pub struct OpusDecoder {
    _private: [u8; 0],
}

unsafe extern "C" {
    // Encoder
    pub fn opus_encoder_get_size(channels: i32) -> i32;
    pub fn opus_encoder_init(st: *mut OpusEncoder, fs: i32, channels: i32, application: i32)
    -> i32;
    pub fn opus_encode(
        st: *mut OpusEncoder,
        pcm: *const i16,
        frame_size: i32,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;

    // Decoder
    pub fn opus_decoder_get_size(channels: i32) -> i32;
    pub fn opus_decoder_init(st: *mut OpusDecoder, fs: i32, channels: i32) -> i32;
    pub fn opus_decode(
        st: *mut OpusDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: i32,
        decode_fec: i32,
    ) -> i32;

    // Encoder CTL — variadic; concrete call sites always pass one i32 value
    pub fn opus_encoder_ctl(st: *mut OpusEncoder, request: i32, ...) -> i32;

    // Decoder CTL — variadic; concrete call sites always pass one i32 value
    pub fn opus_decoder_ctl(st: *mut OpusDecoder, request: i32, ...) -> i32;

    // Utility
    pub fn opus_strerror(error: i32) -> *const core::ffi::c_char;
    pub fn opus_get_version_string() -> *const core::ffi::c_char;
}
