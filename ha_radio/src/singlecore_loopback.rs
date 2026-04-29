#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use {
    board_support::{
        Board, EncoderPio, I2sInputPio, I2sOutputPio,
        consts::{FRAME_SAMPLES, OPUS_BUF_SIZE, OpusPacket, PackedAudioFrame, SAMPLE_RATE},
    },
    defmt::*,
    defmt_rtt as _,
    embassy_executor::Spawner,
    embassy_futures::join::join,
    embassy_rp::{
        bind_interrupts,
        config::Config as SystemConfig,
        dma::InterruptHandler as DmaInterruptHandler,
        i2c::InterruptHandler as I2cInterruptHandler,
        peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, DMA_CH3, I2C0, PIO0, PIO1},
        pio::InterruptHandler as PioInterruptHandler,
    },
    embassy_sync::{
        blocking_mutex::raw::CriticalSectionRawMutex,
        zerocopy_channel::{Channel, Receiver, Sender},
    },
    embassy_time::Timer,
    embedded_opus::{
        Application, DECODER_STATE_SIZE_STEREO, Decoder, ENCODER_STATE_SIZE_STEREO, Encoder,
    },
    panic_probe as _,
    static_cell::StaticCell,
};

bind_interrupts!(struct Irqs0 {
    DMA_IRQ_0 => DmaInterruptHandler<DMA_CH0>, DmaInterruptHandler<DMA_CH1>, DmaInterruptHandler<DMA_CH2>, DmaInterruptHandler<DMA_CH3>;
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    I2C0_IRQ => I2cInterruptHandler<I2C0>;
});

bind_interrupts!(struct Irqs1 {
    PIO1_IRQ_0 => PioInterruptHandler<PIO1>;
});

// ----- TASKS
// Four-stage pipeline:
// Task 1: Capture I2S from PCM1808
// Task 2: Encode with Opus (This will block for ~18 ms)
// Task 3: Decode with Opus (This will block for ~5 ms)
// Task 4: Transmit I2S to PCM1502A
//
// Channels connect the four tasks

type PackedAudioFrameChannel = Channel<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameSender = Sender<'static, CriticalSectionRawMutex, PackedAudioFrame>;
type PackedAudioFrameReceiver = Receiver<'static, CriticalSectionRawMutex, PackedAudioFrame>;

type OpusPacketChannel = Channel<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketSender = Sender<'static, CriticalSectionRawMutex, OpusPacket>;
type OpusPacketReceiver = Receiver<'static, CriticalSectionRawMutex, OpusPacket>;

#[embassy_executor::task]
async fn i2s_in_task(mut i2s: I2sInputPio, mut tx: PackedAudioFrameSender) {
    // Start the I2S input
    i2s.start();
    info!("i2s in: started");
    loop {
        // Grab the next buffer from the channel.
        // If this blocks, the pipeline is behind and the PIO FIFO
        // will overflow (only 166µs of headroom at 48kHz with 8-deep FIFO).
        let t = embassy_time::Instant::now();
        let mut buf = tx.send().await;
        let wait_us = t.elapsed().as_micros();
        if wait_us > 100 {
            warn!("i2s in: stalled {}us waiting for free buffer", wait_us);
        }
        // Start the DMA transfer directly into the buffer
        i2s.read(&mut *buf).await;
        // Notify the channel this buffer has good data in it
        buf.send_done();
    }
}

#[embassy_executor::task]
async fn opus_encode_task(mut rx: PackedAudioFrameReceiver, mut tx: OpusPacketSender) {
    let mut state_buf = [0u8; ENCODER_STATE_SIZE_STEREO];
    let mut encoder =
        Encoder::new(&mut state_buf, SAMPLE_RATE as i32, 2, Application::Audio).unwrap();
    encoder.set_bitrate(64_000).unwrap(); // 64kbps
    encoder.set_complexity(5).unwrap(); // Middle of the road, benched ok
    info!("encode: starting");
    loop {
        // Acquire both slots concurrently to avoid holding the PCM buffer
        // while waiting for a free packet slot
        let (pcm, mut opus) = join(rx.receive(), tx.send()).await;
        // bytemuck reinterprets [u32] as [i16] in-place (zero-copy).
        // Little-endian: each u32 (left<<16)|right becomes [right, left] as i16.
        // Sample VALUES are correct native-endian signed integers.
        // Channel order is [R, L, R, L, ...] — swapped, but Opus takes
        // native-endian i16 (not big-endian), so values are correct.
        // The decode side uses the same layout, making the round-trip consistent.
        let pcm_raw: &[i16] = bytemuck::cast_slice(pcm.as_slice());
        // Encode into the packet buffer (sized to the radio payload limit).
        match encoder.encode(pcm_raw, &mut opus.data) {
            Ok(len) => opus.len = len,
            Err(_e) => {
                error!("encode: failed");
                opus.len = 0;
            }
        }
        // Release PCM buffer first so I2S DMA can reclaim it
        pcm.receive_done();
        opus.send_done();
    }
}

/// Opus decoder — Core 1.  CPU-bound, runs on its own executor so it
/// never starves the I2S output DMA on Core 0.
#[embassy_executor::task]
async fn opus_decode_task(mut rx: OpusPacketReceiver, mut tx: PackedAudioFrameSender) {
    let mut state_buf = [0u8; DECODER_STATE_SIZE_STEREO];
    let mut decoder = Decoder::new(&mut state_buf, SAMPLE_RATE as i32, 2).unwrap();
    info!("decode: starting");
    loop {
        let (opus, mut pcm_slot) = join(rx.receive(), tx.send()).await;
        let pcm: &mut [i16] = bytemuck::cast_slice_mut(&mut (*pcm_slot));
        match decoder.decode(&opus.data[0..opus.len], pcm, false) {
            Ok(_len) => {}
            Err(_e) => {
                error!("decode: failed");
                decoder.plc(pcm).ok();
            }
        }
        opus.receive_done();
        pcm_slot.send_done();
    }
}

#[embassy_executor::task]
async fn i2s_out_task(
    mut i2s: I2sOutputPio,
    mut rx: PackedAudioFrameReceiver,
    mut encoder: EncoderPio,
) {
    let mut apply_volume = async |buf: &mut [u32]| {
        encoder.poll();
        let volume = encoder.pos() as u32; // position constrained above zero, fortunately
        for v in buf {
            // algorithm: give each half of v 16 bits of headroom, left-shift by
            // volume (0 to 16), then reassemble
            let sample = *v;

            let left = (sample as i32) >> 16;
            let right = ((sample & 0xffff) as i16) as i32;

            let left = left << volume;
            let right = right << volume;

            // convert back to unsigned, and reassemble
            *v = ((left as u32) & 0xffff0000) | (((right as u32) >> 16) & 0xffff);
        }
    };

    // Double-buffer: pre-fetch next frame during DMA playback so the
    // gap between DMA transfers is just a function call, not a channel wait.
    let mut buf_a = [0u32; FRAME_SAMPLES / 2];
    let mut buf_b = [0u32; FRAME_SAMPLES / 2];

    // Prime: fill first buffer before starting playback (avoids startup clicks)
    let slot = rx.receive().await;
    buf_a.copy_from_slice(&*slot);
    slot.receive_done();
    apply_volume(&mut buf_a).await;

    i2s.start();
    info!("i2s out: started");

    loop {
        // Play buf_a, pre-fetch into buf_b
        let transfer = i2s.write(&buf_a);
        let slot = rx.receive().await;
        buf_b.copy_from_slice(&*slot);
        slot.receive_done();
        apply_volume(&mut buf_b).await;
        transfer.await;

        // Play buf_b, pre-fetch into buf_a
        let transfer = i2s.write(&buf_b);
        let slot = rx.receive().await;
        buf_a.copy_from_slice(&*slot);
        slot.receive_done();
        apply_volume(&mut buf_a).await;
        transfer.await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // Setup the board
    let mut board = Board::new(SystemConfig::default(), Irqs0, Irqs1);

    // Spin up codec
    board.codec.reset().await.unwrap();
    board.codec.dac_init().await.unwrap();
    board.codec.adc_init().await.unwrap();
    info!("codec configured");

    // ----- Set up the three zero-copy channels
    // The first and last channel will exist on both sides, the middle channel represents the radio

    // I2S -> OPUS
    static I2S_OPUS_BUF: StaticCell<[PackedAudioFrame; 2]> = StaticCell::new();
    let i2s_opus_buf = I2S_OPUS_BUF.init([[0; FRAME_SAMPLES / 2]; 2]);
    static I2S_OPUS_CHAN: StaticCell<PackedAudioFrameChannel> = StaticCell::new();
    let i2s_opus_chan = I2S_OPUS_CHAN.init(Channel::new(i2s_opus_buf));
    let (i2s_opus_tx, i2s_opus_rx) = i2s_opus_chan.split();

    // OPUS -> OPUS
    static ENCODE_DECODE_BUF: StaticCell<[OpusPacket; 2]> = StaticCell::new();
    let encode_decode_buf = ENCODE_DECODE_BUF.init(
        [OpusPacket {
            data: [0; OPUS_BUF_SIZE],
            len: 0,
        }; 2],
    );
    static ENCODE_DECODE_CHAN: StaticCell<OpusPacketChannel> = StaticCell::new();
    let encode_decode_chan = ENCODE_DECODE_CHAN.init(Channel::new(encode_decode_buf));
    let (encode_decode_tx, encode_decode_rx) = encode_decode_chan.split();

    // OPUS -> I2S
    static OPUS_I2S_BUF: StaticCell<[PackedAudioFrame; 2]> = StaticCell::new();
    let opus_i2s_buf = OPUS_I2S_BUF.init([[0; FRAME_SAMPLES / 2]; 2]);
    static OPUS_I2S_CHAN: StaticCell<PackedAudioFrameChannel> = StaticCell::new();
    let opus_i2s_chan = OPUS_I2S_CHAN.init(Channel::new(opus_i2s_buf));
    let (opus_i2s_tx, opus_i2s_rx) = opus_i2s_chan.split();

    // ----- Spawn all the tasks
    spawner.spawn(i2s_in_task(board.i2s_in, i2s_opus_tx).unwrap());
    spawner.spawn(opus_encode_task(i2s_opus_rx, encode_decode_tx).unwrap());
    spawner.spawn(opus_decode_task(encode_decode_rx, opus_i2s_tx).unwrap());
    spawner.spawn(i2s_out_task(board.i2s_out, opus_i2s_rx, board.encoder).unwrap());

    // Output jack-controlled amp control
    info!("loopback: pipeline running, monitoring jack");
    loop {
        if board.out_det.is_low() {
            board.amp_nshdn.set_high();
        } else {
            board.amp_nshdn.set_low();
        }
        Timer::after_millis(50).await;
    }
}
