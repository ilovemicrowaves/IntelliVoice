use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::config::Config;
use crate::dsp::mixer::{db_to_gain, mix_frame};
use crate::dsp::DspPipeline;

/// Run the real-time processing engine.
///
/// Opens CPAL streams on the given devices, spawns a processing thread, and
/// blocks until Ctrl+C.
pub fn run_realtime(
    music_device: Device,
    voice_device: Device,
    output_device: Device,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let sample_rate = config.processing.sample_rate;
    let fft_size = config.processing.fft_size;
    let hop_size = config.hop_size();
    let stream_config = crate::audio::build_stream_config(sample_rate);

    eprintln!("SpectralBlend real-time processor (stereo)");
    eprintln!("  FFT size: {fft_size}, hop: {hop_size}, sample rate: {sample_rate}");
    eprintln!("  Music input:  {}", music_device.name().unwrap_or_default());
    eprintln!("  Voice input:  {}", voice_device.name().unwrap_or_default());
    eprintln!("  Output:       {}", output_device.name().unwrap_or_default());

    // Ring buffer capacity: ~1 second of stereo interleaved f32 samples
    let buf_capacity = (sample_rate as usize) * 2; // *2 for stereo interleaved

    // Music input ring buffer
    let (music_prod, music_cons) = RingBuffer::<f32>::new(buf_capacity);
    // Voice input ring buffer
    let (voice_prod, voice_cons) = RingBuffer::<f32>::new(buf_capacity);
    // Output ring buffer
    let (output_prod, output_cons) = RingBuffer::<f32>::new(buf_capacity);

    // Ctrl+C signal
    let running = Arc::new(AtomicBool::new(true));
    let running_ctrlc = running.clone();
    ctrlc_handler(running_ctrlc);

    // Build CPAL input streams
    let music_stream = build_input_stream(&music_device, &stream_config, music_prod)?;
    let voice_stream = build_input_stream(&voice_device, &stream_config, voice_prod)?;
    let output_stream = build_output_stream(&output_device, &stream_config, output_cons)?;

    // Spawn processing thread
    let running_proc = running.clone();
    let config_clone = config.clone();
    let proc_handle = std::thread::Builder::new()
        .name("dsp-pipeline".into())
        .spawn(move || {
            processing_loop(
                music_cons,
                voice_cons,
                output_prod,
                &config_clone,
                &running_proc,
            );
        })?;

    // Start streams
    music_stream.play()?;
    voice_stream.play()?;
    output_stream.play()?;

    eprintln!("  Processing... (Ctrl+C to stop)");

    // Block until Ctrl+C
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    eprintln!("\nStopping...");

    // Drop streams (stops callbacks)
    drop(music_stream);
    drop(voice_stream);
    drop(output_stream);

    // Wait for processing thread
    let _ = proc_handle.join();

    eprintln!("Done.");
    Ok(())
}

/// Set up a Ctrl+C handler that sets the flag to false.
fn ctrlc_handler(running: Arc<AtomicBool>) {
    // Use a simple approach: spawn a thread that blocks on a signal
    std::thread::Builder::new()
        .name("ctrlc".into())
        .spawn(move || {
            // Block on SIGINT using platform-specific mechanism
            // For simplicity, use a polling approach with libc
            unsafe {
                libc_sigwait_int();
            }
            running.store(false, Ordering::Relaxed);
        })
        .expect("Failed to spawn ctrlc handler thread");
}

/// Block until SIGINT is received (Linux only).
unsafe fn libc_sigwait_int() {
    use std::mem::MaybeUninit;
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    libc::sigemptyset(set.as_mut_ptr());
    libc::sigaddset(set.as_mut_ptr(), libc::SIGINT);
    libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), std::ptr::null_mut());
    let mut sig: i32 = 0;
    libc::sigwait(set.as_ptr(), &mut sig);
}

/// Build a CPAL input stream that pushes interleaved stereo f32 into the ring buffer.
fn build_input_stream(
    device: &Device,
    config: &StreamConfig,
    mut producer: Producer<f32>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // Push as many samples as fit; drop excess on overflow
            let available = producer.slots();
            let to_write = data.len().min(available);
            if to_write > 0 {
                if let Ok(mut chunk) = producer.write_chunk(to_write) {
                    let (first, second) = chunk.as_mut_slices();
                    let first_len = first.len().min(to_write);
                    first[..first_len].copy_from_slice(&data[..first_len]);
                    if to_write > first_len {
                        let remainder = to_write - first_len;
                        second[..remainder].copy_from_slice(&data[first_len..first_len + remainder]);
                    }
                    chunk.commit_all();
                }
            }
        },
        |err| eprintln!("Input stream error: {err}"),
        None,
    )?;
    Ok(stream)
}

/// Build a CPAL output stream that pulls interleaved stereo f32 from the ring buffer.
fn build_output_stream(
    device: &Device,
    config: &StreamConfig,
    mut consumer: Consumer<f32>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let available = consumer.slots();
            let to_read = data.len().min(available);
            if to_read > 0 {
                if let Ok(chunk) = consumer.read_chunk(to_read) {
                    let (first, second) = chunk.as_slices();
                    data[..first.len()].copy_from_slice(first);
                    if !second.is_empty() {
                        data[first.len()..first.len() + second.len()].copy_from_slice(second);
                    }
                    let total = first.len() + second.len();
                    chunk.commit_all();
                    // Fill any remaining with silence
                    for s in &mut data[total..] {
                        *s = 0.0;
                    }
                } else {
                    // Silence on error
                    data.fill(0.0);
                }
            } else {
                // Silence on underrun
                data.fill(0.0);
            }
        },
        |err| eprintln!("Output stream error: {err}"),
        None,
    )?;
    Ok(stream)
}

/// The main DSP processing loop running on a dedicated thread.
///
/// Consumes from music and voice ring buffers, runs the full pipeline,
/// and pushes mixed output into the output ring buffer.
fn processing_loop(
    mut music_cons: Consumer<f32>,
    mut voice_cons: Consumer<f32>,
    mut output_prod: Producer<f32>,
    config: &Config,
    running: &AtomicBool,
) {
    let fft_size = config.processing.fft_size;
    let hop_size = config.hop_size();
    let hop_stereo = hop_size * 2; // interleaved stereo samples per hop

    let mut pipeline = DspPipeline::new(config);

    // Sliding windows: last fft_size samples per channel
    let mut music_window_l = vec![0.0f32; fft_size];
    let mut music_window_r = vec![0.0f32; fft_size];
    let mut voice_window_l = vec![0.0f32; fft_size];
    let mut voice_window_r = vec![0.0f32; fft_size];

    // Overlap-add accumulators (fft_size each)
    let mut music_accum_l = vec![0.0f32; fft_size];
    let mut music_accum_r = vec![0.0f32; fft_size];
    let mut voice_accum_l = vec![0.0f32; fft_size];
    let mut voice_accum_r = vec![0.0f32; fft_size];

    // Hop buffers for de-interleaving input
    let mut hop_buf = vec![0.0f32; hop_stereo];
    let mut hop_l = vec![0.0f32; hop_size];
    let mut hop_r = vec![0.0f32; hop_size];

    // Output mix buffer (hop_size per channel)
    let mut mix_out_l = vec![0.0f32; hop_size];
    let mut mix_out_r = vec![0.0f32; hop_size];
    let mut interleaved_out = vec![0.0f32; hop_stereo];

    let music_gain = db_to_gain(config.output.music_gain_db);
    let voice_gain = db_to_gain(config.output.voice_gain_db);

    while running.load(Ordering::Relaxed) {
        // Wait for enough samples from both inputs
        if music_cons.slots() < hop_stereo || voice_cons.slots() < hop_stereo {
            std::thread::sleep(std::time::Duration::from_micros(500));
            continue;
        }

        // Read music hop
        read_hop(&mut music_cons, &mut hop_buf, &mut hop_l, &mut hop_r, hop_size);
        // Shift window left by hop_size, append new samples
        shift_and_append(&mut music_window_l, &hop_l, hop_size);
        shift_and_append(&mut music_window_r, &hop_r, hop_size);

        // Read voice hop
        read_hop(&mut voice_cons, &mut hop_buf, &mut hop_l, &mut hop_r, hop_size);
        shift_and_append(&mut voice_window_l, &hop_l, hop_size);
        shift_and_append(&mut voice_window_r, &hop_r, hop_size);

        // Zero the accumulators before processing this frame
        music_accum_l.fill(0.0);
        music_accum_r.fill(0.0);
        voice_accum_l.fill(0.0);
        voice_accum_r.fill(0.0);

        // Run the full DSP pipeline (position=0 since accumulators are local)
        pipeline.process_frame(
            &music_window_l,
            &music_window_r,
            &voice_window_l,
            &voice_window_r,
            &config.masking,
            &mut music_accum_l,
            &mut music_accum_r,
            &mut voice_accum_l,
            &mut voice_accum_r,
            0,
            1.0, // full compression
        );

        // Read first hop_size samples from accumulators → mix
        mix_frame(
            &music_accum_l[..hop_size],
            &voice_accum_l[..hop_size],
            music_gain,
            voice_gain,
            &mut mix_out_l,
        );
        mix_frame(
            &music_accum_r[..hop_size],
            &voice_accum_r[..hop_size],
            music_gain,
            voice_gain,
            &mut mix_out_r,
        );

        // Interleave L/R for output
        for i in 0..hop_size {
            interleaved_out[i * 2] = mix_out_l[i];
            interleaved_out[i * 2 + 1] = mix_out_r[i];
        }

        // Push to output ring buffer (drop excess on overflow)
        let available = output_prod.slots();
        let to_write = interleaved_out.len().min(available);
        if to_write > 0 {
            if let Ok(mut chunk) = output_prod.write_chunk(to_write) {
                let (first, second) = chunk.as_mut_slices();
                let first_len = first.len();
                first.copy_from_slice(&interleaved_out[..first_len]);
                if !second.is_empty() {
                    let second_len = second.len();
                    second.copy_from_slice(&interleaved_out[first_len..first_len + second_len]);
                }
                chunk.commit_all();
            }
        }
    }
}

/// Read hop_stereo interleaved samples from the consumer, de-interleave into L/R.
fn read_hop(
    consumer: &mut Consumer<f32>,
    hop_buf: &mut [f32],
    hop_l: &mut [f32],
    hop_r: &mut [f32],
    hop_size: usize,
) {
    let hop_stereo = hop_size * 2;
    if let Ok(chunk) = consumer.read_chunk(hop_stereo) {
        let (first, second) = chunk.as_slices();
        hop_buf[..first.len()].copy_from_slice(first);
        if !second.is_empty() {
            hop_buf[first.len()..first.len() + second.len()].copy_from_slice(second);
        }
        chunk.commit_all();
    }

    // De-interleave
    for i in 0..hop_size {
        hop_l[i] = hop_buf[i * 2];
        hop_r[i] = hop_buf[i * 2 + 1];
    }
}

/// Shift a window left by `hop_size` and append new samples at the end.
fn shift_and_append(window: &mut [f32], new_samples: &[f32], hop_size: usize) {
    let fft_size = window.len();
    window.copy_within(hop_size..fft_size, 0);
    window[fft_size - hop_size..].copy_from_slice(new_samples);
}
