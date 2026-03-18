//! This build script copies the `memory.x` file from the crate root into
//! a directory where the linker can always find it at build time.
//! For many projects this is optional, as the linker always searches the
//! project root directory -- wherever `Cargo.toml` is. However, if you
//! are using a workspace or have a more complicated build setup, this
//! build script becomes required. Additionally, by requesting that
//! Cargo re-run the build script whenever `memory.x` is changed,
//! updating `memory.x` ensures a rebuild of the application with the
//! new memory settings.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // Put `memory.x` in our output directory and ensure it's on the linker search path.
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");

    // Generate 1 second of stereo interleaved i16 PCM at 48 kHz.
    // L = 440 Hz (A4), R = 554 Hz (C#5) — a minor third, gives opus something
    // realistic to encode rather than a trivially-correlated stereo field.
    const SAMPLE_RATE: usize = 48_000;
    const AMPLITUDE: f64 = i16::MAX as f64 * 0.8;
    const FREQ_L: f64 = 440.0;
    const FREQ_R: f64 = 554.365;

    let samples: Vec<u8> = (0..SAMPLE_RATE)
        .flat_map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            let l = (AMPLITUDE * (2.0 * std::f64::consts::PI * FREQ_L * t).sin()) as i16;
            let r = (AMPLITUDE * (2.0 * std::f64::consts::PI * FREQ_R * t).sin()) as i16;
            l.to_le_bytes().into_iter().chain(r.to_le_bytes())
        })
        .collect();

    File::create(out.join("bench_pcm.raw"))
        .unwrap()
        .write_all(&samples)
        .unwrap();
}
