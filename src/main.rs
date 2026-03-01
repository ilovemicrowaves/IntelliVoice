mod config;
mod dsp;

use clap::{Parser, Subcommand};
use config::Config;
use dsp::{OverlapAddProcessor, SpectralMask, VoiceCompressor, VoiceGate, mix_frame, mixer::db_to_gain};
use rustfft::num_complex::Complex;
use std::path::{Path, PathBuf};

type WavResult = Result<(Vec<f32>, Vec<f32>, u32), Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "spectralblend", about = "Spectral voice masking audio processor")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Process WAV files offline (for testing and tuning)
    Offline {
        /// Path to the music WAV file
        #[arg(long)]
        music: PathBuf,
        /// Path to the voice WAV file
        #[arg(long)]
        voice: PathBuf,
        /// Path to the output WAV file
        #[arg(long)]
        output: PathBuf,
        /// Path to config TOML file (uses defaults if omitted)
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Offline {
            music,
            voice,
            output,
            config,
        } => {
            let cfg = match config {
                Some(path) => Config::load(&path).unwrap_or_else(|e| {
                    eprintln!("Failed to load config: {e}");
                    std::process::exit(1);
                }),
                None => Config::default(),
            };
            if let Err(e) = process_offline(&music, &voice, &output, &cfg) {
                eprintln!("Processing failed: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn read_wav_stereo(path: &Path) -> WavResult {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels as usize;

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            let bits = spec.bits_per_sample;
            let max_val = (1u32 << (bits - 1)) as f32;
            reader
                .into_samples::<i32>()
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|s| s as f32 / max_val)
                .collect()
        }
    };

    let (left, right) = if channels == 1 {
        // Mono: duplicate to both channels
        (samples.clone(), samples)
    } else {
        // Stereo or multi-channel: de-interleave first two channels
        let mut l = Vec::with_capacity(samples.len() / channels);
        let mut r = Vec::with_capacity(samples.len() / channels);
        for frame in samples.chunks(channels) {
            l.push(frame[0]);
            r.push(frame[1]);
        }
        (l, r)
    };

    Ok((left, right, sample_rate))
}

fn write_wav_stereo(
    path: &Path,
    left: &[f32],
    right: &[f32],
    sample_rate: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for (&l, &r) in left.iter().zip(right.iter()) {
        writer.write_sample(l)?;
        writer.write_sample(r)?;
    }
    writer.finalize()?;
    Ok(())
}

fn process_offline(
    music_path: &Path,
    voice_path: &Path,
    output_path: &Path,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let fft_size = config.processing.fft_size;
    let hop_size = config.hop_size();
    let sample_rate = config.processing.sample_rate;

    eprintln!("SpectralBlend offline processor (stereo)");
    eprintln!("  FFT size: {fft_size}, hop: {hop_size}, sample rate: {sample_rate}");

    // Read input files as stereo (mono inputs get duplicated to both channels)
    let (music_l, music_r, music_sr) = read_wav_stereo(music_path)?;
    let (voice_l, voice_r, voice_sr) = read_wav_stereo(voice_path)?;

    if music_sr != sample_rate {
        return Err(format!(
            "Music sample rate ({music_sr}) doesn't match config ({sample_rate})"
        ).into());
    }
    if voice_sr != sample_rate {
        return Err(format!(
            "Voice sample rate ({voice_sr}) doesn't match config ({sample_rate})"
        ).into());
    }

    // Pad shorter to match longer
    let total_len = music_l.len().max(voice_l.len());
    let mut music_l = music_l;
    let mut music_r = music_r;
    let mut voice_l = voice_l;
    let mut voice_r = voice_r;
    music_l.resize(total_len, 0.0);
    music_r.resize(total_len, 0.0);
    voice_l.resize(total_len, 0.0);
    voice_r.resize(total_len, 0.0);

    eprintln!(
        "  Music: {} samples ({:.1}s), Voice: {} samples ({:.1}s)",
        total_len,
        total_len as f32 / sample_rate as f32,
        total_len,
        total_len as f32 / sample_rate as f32,
    );

    // Per-channel processors (L/R)
    let mut music_fft_l = OverlapAddProcessor::new(fft_size);
    let mut music_fft_r = OverlapAddProcessor::new(fft_size);
    let mut voice_fft_l = OverlapAddProcessor::new(fft_size);
    let mut voice_fft_r = OverlapAddProcessor::new(fft_size);
    let mut spectral_mask_l = SpectralMask::new(fft_size, sample_rate);
    let mut spectral_mask_r = SpectralMask::new(fft_size, sample_rate);
    let mut voice_compressor_l = VoiceCompressor::new(0.15, 0.002, 1.8, sample_rate, hop_size);
    let mut voice_compressor_r = VoiceCompressor::new(0.15, 0.002, 1.8, sample_rate, hop_size);

    // Shared voice gate (person is speaking or not — same decision for both channels)
    let mut voice_gate = VoiceGate::new(
        config.envelope.gate_threshold_on,
        config.envelope.gate_threshold_off,
        config.envelope.attack_ms,
        config.envelope.release_ms,
        sample_rate,
        hop_size,
    );

    // Output accumulators (overlap-add targets), one per channel per stream
    let output_len = total_len + fft_size;
    let mut masked_music_accum_l = vec![0.0f32; output_len];
    let mut masked_music_accum_r = vec![0.0f32; output_len];
    let mut voice_accum_l = vec![0.0f32; output_len];
    let mut voice_accum_r = vec![0.0f32; output_len];

    // Tracking stats
    let mut frame_count = 0u64;
    let mut mask_sum = 0.0f64;
    let mut mask_min = 1.0f32;

    // Processing loop
    let mut pos = 0;
    while pos + fft_size <= total_len {
        let music_frame_l = &music_l[pos..pos + fft_size];
        let music_frame_r = &music_r[pos..pos + fft_size];
        let voice_frame_l = &voice_l[pos..pos + fft_size];
        let voice_frame_r = &voice_r[pos..pos + fft_size];

        // Compress voice L/R independently
        let compressed_voice_l = voice_compressor_l.process(voice_frame_l);
        let compressed_voice_r = voice_compressor_r.process(voice_frame_r);

        // Forward FFT all four streams
        let music_spectrum_l = music_fft_l.process_frame(music_frame_l).to_vec();
        let music_spectrum_r = music_fft_r.process_frame(music_frame_r).to_vec();
        let voice_spectrum_l = voice_fft_l.process_frame(&compressed_voice_l).to_vec();
        let voice_spectrum_r = voice_fft_r.process_frame(&compressed_voice_r).to_vec();

        // Shared voice gate from both channels
        let gate_value = voice_gate.update_stereo(&voice_spectrum_l, &voice_spectrum_r);

        // Build masks independently per channel (same gate value)
        let music_mag_l: Vec<f32> = music_spectrum_l.iter().map(|c| c.norm()).collect();
        let voice_mag_l: Vec<f32> = voice_spectrum_l.iter().map(|c| c.norm()).collect();
        let mask_l = spectral_mask_l.build_mask(&voice_mag_l, &music_mag_l, gate_value, &config.masking);

        let music_mag_r: Vec<f32> = music_spectrum_r.iter().map(|c| c.norm()).collect();
        let voice_mag_r: Vec<f32> = voice_spectrum_r.iter().map(|c| c.norm()).collect();
        let mask_r = spectral_mask_r.build_mask(&voice_mag_r, &music_mag_r, gate_value, &config.masking);

        // Apply masks to music spectra
        let masked_spectrum_l: Vec<Complex<f32>> = music_spectrum_l
            .iter()
            .zip(mask_l.iter())
            .map(|(bin, &m)| bin * m)
            .collect();
        let masked_spectrum_r: Vec<Complex<f32>> = music_spectrum_r
            .iter()
            .zip(mask_r.iter())
            .map(|(bin, &m)| bin * m)
            .collect();

        // Track stats (average of both channels)
        let avg_l: f32 = mask_l.iter().sum::<f32>() / mask_l.len() as f32;
        let avg_r: f32 = mask_r.iter().sum::<f32>() / mask_r.len() as f32;
        mask_sum += ((avg_l + avg_r) * 0.5) as f64;
        let min_l = *mask_l.iter().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(&1.0);
        let min_r = *mask_r.iter().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(&1.0);
        mask_min = mask_min.min(min_l).min(min_r);
        frame_count += 1;

        // IFFT + overlap-add per channel
        music_fft_l.synthesize(&masked_spectrum_l, &mut masked_music_accum_l, pos);
        music_fft_r.synthesize(&masked_spectrum_r, &mut masked_music_accum_r, pos);
        voice_fft_l.synthesize(&voice_spectrum_l, &mut voice_accum_l, pos);
        voice_fft_r.synthesize(&voice_spectrum_r, &mut voice_accum_r, pos);

        pos += hop_size;
    }

    // Mix per channel
    let music_gain = db_to_gain(config.output.music_gain_db);
    let voice_gain = db_to_gain(config.output.voice_gain_db);

    let mut output_l = vec![0.0f32; total_len];
    let mut output_r = vec![0.0f32; total_len];
    mix_frame(
        &masked_music_accum_l[..total_len],
        &voice_accum_l[..total_len],
        music_gain,
        voice_gain,
        &mut output_l,
    );
    mix_frame(
        &masked_music_accum_r[..total_len],
        &voice_accum_r[..total_len],
        music_gain,
        voice_gain,
        &mut output_r,
    );

    // Write stereo output
    write_wav_stereo(output_path, &output_l, &output_r, sample_rate)?;

    // Print stats
    let avg_mask = if frame_count > 0 {
        mask_sum / frame_count as f64
    } else {
        1.0
    };
    eprintln!("  Frames processed: {frame_count}");
    eprintln!("  Average mask value: {avg_mask:.4}");
    eprintln!("  Min mask value: {mask_min:.4} ({:.1} dB)", 20.0 * mask_min.max(1e-10).log10());
    eprintln!("  Output written to: {}", output_path.display());

    Ok(())
}
