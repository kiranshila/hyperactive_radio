#![no_std]
#![no_main]
#![forbid(unsafe_code)]
#![feature(impl_trait_in_assoc_type)]

use cortex_m::peripheral::DWT;
use defmt::*;
use embassy_executor::Spawner;
use embassy_rp::clocks::ClockConfig;
use embassy_rp::config::Config;
use embedded_opus::{
    Application, DECODER_STATE_SIZE_STEREO, Decoder, ENCODER_STATE_SIZE_STEREO, Encoder,
};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

const CLOCK_MHZ: u32 = 270;
const SAMPLE_RATE: i32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_SAMPLES: usize = 960 * CHANNELS; // 20ms stereo

const TARGET_BITRATE: i32 = 64_000;

// 1 second of stereo interleaved i16 PCM at 48 kHz, −6 dBFS broadband noise.
//
// We use broadband noise rather than sine waves because pure tones are
// artificially easy for Opus: clear pitch lets SILK converge fast, sparse
// spectrum gives CELT less work. Broadband noise is the conservative
// worst case — hardest for SILK pitch analysis, hardest for CELT spectral
// coding — giving credible upper-bound timing for margin planning.
//
// LCG PRNG with fixed seeds so the benchmark is reproducible across builds.
// Amplitude scaling: i32 → i16 via arithmetic right-shift by 17 gives −6 dBFS.
const fn generate_bench_pcm() -> [i16; 48_000 * 2] {
    let mut out = [0i16; 48_000 * 2];
    let mut state_l: u32 = 0xDEAD_BEEF;
    let mut state_r: u32 = 0xB0BA_CAFE;
    let mut i = 0usize;
    while i < 48_000 {
        state_l = state_l.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state_r = state_r.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out[i * 2] = (state_l as i32 >> 17) as i16;
        out[i * 2 + 1] = (state_r as i32 >> 17) as i16;
        i += 1;
    }
    out
}

static BENCH_PCM: [i16; 48_000 * 2] = generate_bench_pcm();

static ENCODER_STATE: StaticCell<[u8; ENCODER_STATE_SIZE_STEREO]> = StaticCell::new();
static DECODER_STATE: StaticCell<[u8; DECODER_STATE_SIZE_STEREO]> = StaticCell::new();

// DWT.CYCCNT ticks at the CPU clock
fn bench<F: FnMut()>(name: &str, iterations: u32, mut body: F) {
    let mut min = u32::MAX;
    let mut max = u32::MIN;
    let mut total: u64 = 0;

    for _ in 0..iterations {
        let start = DWT::cycle_count();
        body();
        let elapsed = DWT::cycle_count().wrapping_sub(start);
        if elapsed < min {
            min = elapsed;
        }
        if elapsed > max {
            max = elapsed;
        }
        total += elapsed as u64;
    }

    let avg = (total / iterations as u64) as u32;
    info!(
        "[bench] {}: min={} avg={} max={} cycles ({} µs avg)",
        name,
        min,
        avg,
        max,
        avg / CLOCK_MHZ
    );
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut config = Config::default();
    config.clocks = ClockConfig::system_freq(CLOCK_MHZ * 1_000_000).unwrap();
    let _p = embassy_rp::init(config);

    let mut core = cortex_m::Peripherals::take().unwrap();
    core.DCB.enable_trace();
    core.DWT.enable_cycle_counter();

    let pcm_samples: &[i16] = &BENCH_PCM;

    let mut encoder = Encoder::new(
        ENCODER_STATE.init([0; ENCODER_STATE_SIZE_STEREO]),
        SAMPLE_RATE,
        CHANNELS,
        Application::Audio,
    )
    .unwrap();
    encoder.set_bitrate(TARGET_BITRATE).unwrap();
    encoder.set_complexity(5).unwrap();

    info!("--- benchmarks start ---");

    let total_frames = pcm_samples.len() / FRAME_SAMPLES;
    let mut frame_idx = 0;
    let mut packet_buf = [0u8; 4000];

    bench("opus_encode_20ms_stereo_48k_64kbps", 50, || {
        let frame = &pcm_samples[frame_idx * FRAME_SAMPLES..(frame_idx + 1) * FRAME_SAMPLES];
        encoder.encode(frame, &mut packet_buf).unwrap();
        frame_idx = (frame_idx + 1) % total_frames;
    });

    let packet_len = {
        let frame = &pcm_samples[0..FRAME_SAMPLES];
        encoder.encode(frame, &mut packet_buf).unwrap()
    };

    let mut decoder = Decoder::new(
        DECODER_STATE.init([0; DECODER_STATE_SIZE_STEREO]),
        SAMPLE_RATE,
        CHANNELS,
    )
    .unwrap();

    let mut pcm_out = [0i16; FRAME_SAMPLES];

    bench("opus_decode_20ms_stereo_48k_64kbps", 50, || {
        decoder
            .decode(&packet_buf[..packet_len], &mut pcm_out, false)
            .unwrap();
    });

    info!("--- benchmarks done ---");
}
