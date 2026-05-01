#![no_std]

use opus_sys as sys;

include!(concat!(env!("OUT_DIR"), "/opus_sizes.rs"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadArg,
    BufferTooSmall,
    InternalError,
    InvalidPacket,
    Unimplemented,
    InvalidState,
    AllocFail,
    Unknown(i32),
}

impl Error {
    fn from_code(code: i32) -> Self {
        match code {
            sys::OPUS_BAD_ARG => Error::BadArg,
            sys::OPUS_BUFFER_TOO_SMALL => Error::BufferTooSmall,
            sys::OPUS_INTERNAL_ERROR => Error::InternalError,
            sys::OPUS_INVALID_PACKET => Error::InvalidPacket,
            sys::OPUS_UNIMPLEMENTED => Error::Unimplemented,
            sys::OPUS_INVALID_STATE => Error::InvalidState,
            sys::OPUS_ALLOC_FAIL => Error::AllocFail,
            other => Error::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    Voip,
    Audio,
    RestrictedLowdelay,
}

impl Application {
    fn to_sys(self) -> i32 {
        match self {
            Application::Voip => sys::OPUS_APPLICATION_VOIP,
            Application::Audio => sys::OPUS_APPLICATION_AUDIO,
            Application::RestrictedLowdelay => sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        }
    }
}

pub struct Encoder<'buf> {
    state: &'buf mut sys::OpusEncoder,
    channels: usize,
}

impl<'buf> Encoder<'buf> {
    pub fn new(
        buf: &'buf mut [u8],
        sample_rate: i32,
        channels: usize,
        application: Application,
    ) -> Result<Self, Error> {
        let state = unsafe {
            let required = sys::opus_encoder_get_size(channels as i32) as usize;
            if buf.len() < required {
                return Err(Error::BufferTooSmall);
            }
            let ptr = buf.as_mut_ptr() as *mut sys::OpusEncoder;
            let ret =
                sys::opus_encoder_init(ptr, sample_rate, channels as i32, application.to_sys());
            if ret != sys::OPUS_OK {
                return Err(Error::from_code(ret));
            }
            &mut *ptr
        };
        Ok(Self { state, channels })
    }

    pub fn set_bitrate(&mut self, bps: i32) -> Result<(), Error> {
        let ret = unsafe { sys::opus_encoder_ctl(self.state, sys::OPUS_SET_BITRATE_REQUEST, bps) };
        if ret != sys::OPUS_OK {
            Err(Error::from_code(ret))
        } else {
            Ok(())
        }
    }

    pub fn set_complexity(&mut self, complexity: i32) -> Result<(), Error> {
        let ret = unsafe {
            sys::opus_encoder_ctl(self.state, sys::OPUS_SET_COMPLEXITY_REQUEST, complexity)
        };
        if ret != sys::OPUS_OK {
            Err(Error::from_code(ret))
        } else {
            Ok(())
        }
    }

    pub fn set_inband_fec(&mut self, enabled: bool) -> Result<(), Error> {
        let ret = unsafe {
            sys::opus_encoder_ctl(self.state, sys::OPUS_SET_INBAND_FEC_REQUEST, enabled as i32)
        };
        if ret != sys::OPUS_OK {
            Err(Error::from_code(ret))
        } else {
            Ok(())
        }
    }

    pub fn set_packet_loss_perc(&mut self, percentage: i32) -> Result<(), Error> {
        let ret = unsafe {
            sys::opus_encoder_ctl(
                self.state,
                sys::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
                percentage,
            )
        };
        if ret != sys::OPUS_OK {
            Err(Error::from_code(ret))
        } else {
            Ok(())
        }
    }

    pub fn encode(&mut self, pcm: &[i16], output: &mut [u8]) -> Result<usize, Error> {
        let frame_size = (pcm.len() / self.channels) as i32;
        let ret = unsafe {
            sys::opus_encode(
                self.state,
                pcm.as_ptr(),
                frame_size,
                output.as_mut_ptr(),
                output.len() as i32,
            )
        };
        if ret < 0 {
            Err(Error::from_code(ret))
        } else {
            Ok(ret as usize)
        }
    }
}

pub struct Decoder<'buf> {
    state: &'buf mut sys::OpusDecoder,
    channels: usize,
}

impl<'buf> Decoder<'buf> {
    pub fn new(buf: &'buf mut [u8], sample_rate: i32, channels: usize) -> Result<Self, Error> {
        let state = unsafe {
            let required = sys::opus_decoder_get_size(channels as i32) as usize;
            if buf.len() < required {
                return Err(Error::BufferTooSmall);
            }
            let ptr = buf.as_mut_ptr() as *mut sys::OpusDecoder;
            let ret = sys::opus_decoder_init(ptr, sample_rate, channels as i32);
            if ret != sys::OPUS_OK {
                return Err(Error::from_code(ret));
            }
            &mut *ptr
        };
        Ok(Self { state, channels })
    }

    pub fn set_ignore_extensions(&mut self, ignore: bool) -> Result<(), Error> {
        let ret = unsafe {
            sys::opus_decoder_ctl(
                self.state,
                sys::OPUS_SET_IGNORE_EXTENSIONS_REQUEST,
                ignore as i32,
            )
        };
        if ret != sys::OPUS_OK {
            Err(Error::from_code(ret))
        } else {
            Ok(())
        }
    }

    /// Decode an Opus packet into PCM samples.
    /// An empty `packet` slice triggers Packet Loss Concealment (PLC),
    /// which interpolates audio from the decoder's internal state.
    pub fn decode(&mut self, packet: &[u8], pcm: &mut [i16], fec: bool) -> Result<usize, Error> {
        let frame_size = (pcm.len() / self.channels) as i32;
        let (ptr, len) = if packet.is_empty() {
            (core::ptr::null(), 0)
        } else {
            (packet.as_ptr(), packet.len() as i32)
        };
        let ret = unsafe {
            sys::opus_decode(
                self.state,
                ptr,
                len,
                pcm.as_mut_ptr(),
                frame_size,
                fec as i32,
            )
        };
        if ret < 0 {
            Err(Error::from_code(ret))
        } else {
            Ok(ret as usize)
        }
    }

    /// Generate a frame with "packet loss concealment"
    pub fn plc(&mut self, pcm: &mut [i16]) -> Result<usize, Error> {
        let frame_size = (pcm.len() / self.channels) as i32;
        let ret = unsafe {
            sys::opus_decode(
                self.state,
                core::ptr::null(),
                0,
                pcm.as_mut_ptr(),
                frame_size,
                0,
            )
        };
        if ret < 0 {
            Err(Error::from_code(ret))
        } else {
            Ok(ret as usize)
        }
    }
}
