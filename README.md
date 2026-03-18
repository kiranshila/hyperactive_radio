# Hyperactive//2026 Radio

Experimental real-time, long-range, high-quality, wireless digital audio for Hyperactive 2026.

## Audio quality

| Parameter | Value |
|-----------|-------|
| Codec | Opus (fixed-point) |
| Bitrate | 64 kbps |
| Channels | Stereo |
| Sample rate | 48 kHz |
| Frame size | 20 ms |
| Encoder complexity | 5 / 10 |
| ADC | PCM1808 (24-bit I2S) |

64 kbps stereo Opus at 48 kHz should be transparent for most program material — comparable to a good 128 kbps MP3, but with lower latency and better behaviour at the edges of the bitrate.
The fixed-point build removes float entirely; no performance penalty on Cortex-M33 which has a hardware FPU but where the float Opus path was benchmarked to be slower than fixed-point.

End-to-end latency is approximately one frame (20 ms) of algorithmic delay plus radio airtime (~1.6 ms at 100 kbps for a 160-byte packet) plus any playout buffer on the receiver.

## Radio

| Parameter | Value |
|-----------|-------|
| Radio IC | SX1276 |
| Band | 915 MHz (US ISM) |
| Modulation | GFSK |
| Air rate | 100 kbps |
| TX power | +20 dBm (100 mW) |
| Target range | ~300 m / ~1000 ft mixed indoor/outdoor |
| Link margin | ~18 dB at 100 kbps GFSK |

### Regulatory (FCC Part 15.247)

No frequency hopping required. Part 15.247 has two compliance paths — FHSS is one, but **digital modulation** is the other: ≥ 500 kHz of 6 dB occupied bandwidth, up to +30 dBm EIRP, fixed frequency, no duty cycle restriction.
GFSK at 100 kbps with ±50 kHz deviation clears 500 kHz easily, so the system sits on a single channel continuously at +20 dBm and is fully unlicensed-legal.

### Packet structure

Each 20 ms Opus frame encodes to roughly 160 bytes at 64 kbps.
With ~10 bytes of framing (sequence number, network ID, CRC) the total packet is ~170 bytes, for an effective air rate of ~68 kbps — well within the 100 kbps channel.

## Performance

MCU runs at **270 MHz** (overclocked from the 150 MHz default; stable at stock 1.1 V).

Benchmarked on hardware against broadband LCG noise (worst case for Opus — no clear pitch, flat spectrum):

| Operation | Cycles (avg) | Time (avg) | Frame budget used |
|-----------|-------------|------------|-------------------|
| Opus encode, stereo, 64 kbps, 20 ms | 2,299,603 | 8.5 ms | **42.6%** |
| Opus decode, stereo, 64 kbps, 20 ms | 895,420 | 3.3 ms | **16.6%** |

Frame budget = 20 ms = 5,400,000 cycles at 270 MHz.

The encoder leaves 11.5 ms of headroom per frame for I2S DMA and SPI radio management.
The decoder uses only 16.6% of the frame, leaving ample margin for playout buffering and output DMA.

## Firmware architecture

Written in Rust (`no_std`, Embassy async runtime).

```
ha_radio/          # firmware binary (bench + main)
embedded-opus/     # safe Rust wrapper around libopus
opus-sys/          # raw FFI bindings; builds libopus via cc crate
```

`embedded-opus/build.rs` compiles a host-side C probe at build time to determine the exact encoder and decoder state sizes, so buffers are sized precisely without a runtime heap.

### Planned: dual-core pipeline

The encode headroom is comfortable at 270 MHz, but the dual-core split is still planned for clean separation of concerns once the ADC arrives:

- **Core 0** — I2S DMA capture, SX1276 SPI, radio management
- **Core 1** — Opus encode only

A pair of Embassy `Channel` queues (depth 2) connect the cores: PCM frames inbound, encoded packets outbound.

## Build

```sh
cargo build --release --bin ha_radio
cargo build --release --bin bench
```

Requires a Rust nightly toolchain and the `thumbv8m.main-none-eabihf` target (see `rust-toolchain.toml`).
