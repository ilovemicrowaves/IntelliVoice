mod config;
mod dsp;

use clap::{Parser, Subcommand};
use config::Config;
use dsp::{OverlapAddProcessor, SpectralMask, VoiceGate, mix_frame, mixer::db_to_gain};
use rustfft::num_complex::Complex;
use std::path::{Path, PathBuf};

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

fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32), Box<dyn std::error::Error>> {
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

    // Downmix to mono if stereo
    let mono = if channels == 1 {
        samples
    } else {
        samples
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    Ok((mono, sample_rate))
}

fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), Box<dyn std::error::Error>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample(s)?;
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

    eprintln!("SpectralBlend offline processor");
    eprintln!("  FFT size: {fft_size}, hop: {hop_size}, sample rate: {sample_rate}");

    // Read input files
    let (music_samples, music_sr) = read_wav_mono(music_path)?;
    let (voice_samples, voice_sr) = read_wav_mono(voice_path)?;

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
    let total_len = music_samples.len().max(voice_samples.len());
    let mut music = music_samples;
    let mut voice = voice_samples;
    music.resize(total_len, 0.0);
    voice.resize(total_len, 0.0);

    eprintln!(
        "  Music: {} samples ({:.1}s), Voice: {} samples ({:.1}s)",
        music.len(),
        music.len() as f32 / sample_rate as f32,
        voice.len(),
        voice.len() as f32 / sample_rate as f32,
    );

    // Allocate processors
    let mut music_fft = OverlapAddProcessor::new(fft_size);
    let mut voice_fft = OverlapAddProcessor::new(fft_size);
    let mut voice_gate = VoiceGate::new(
        config.envelope.gate_threshold_on,
        config.envelope.gate_threshold_off,
        config.envelope.attack_ms,
        config.envelope.release_ms,
        sample_rate,
        hop_size,
    );
    let mut spectral_mask = SpectralMask::new(fft_size, sample_rate);

    // Output accumulators (overlap-add targets)
    let output_len = total_len + fft_size; // extra padding for overlap-add tail
    let mut masked_music_accum = vec![0.0f32; output_len];
    let mut voice_accum = vec![0.0f32; output_len];

    // Tracking stats
    let mut frame_count = 0u64;
    let mut mask_sum = 0.0f64;
    let mut mask_min = 1.0f32;

    // Processing loop
    let mut pos = 0;
    while pos + fft_size <= total_len {
        let music_frame = &music[pos..pos + fft_size];
        let voice_frame = &voice[pos..pos + fft_size];

        // Forward FFT both
        let music_spectrum = music_fft.process_frame(music_frame).to_vec();
        let voice_spectrum = voice_fft.process_frame(voice_frame).to_vec();

        // Extract magnitudes
        let music_mag: Vec<f32> = music_spectrum.iter().map(|c| c.norm()).collect();
        let voice_mag: Vec<f32> = voice_spectrum.iter().map(|c| c.norm()).collect();

        // Update voice gate
        let gate_value = voice_gate.update(&voice_spectrum);

        // Build spectral mask
        let mask = spectral_mask.build_mask(&voice_mag, &music_mag, gate_value, &config.masking);

        // Apply mask to music spectrum
        let masked_spectrum: Vec<Complex<f32>> = music_spectrum
            .iter()
            .zip(mask.iter())
            .map(|(bin, &m)| bin * m)
            .collect();

        // Track stats
        let frame_mask_avg: f32 = mask.iter().sum::<f32>() / mask.len() as f32;
        mask_sum += frame_mask_avg as f64;
        mask_min = mask_min.min(*mask.iter().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(&1.0));
        frame_count += 1;

        // IFFT + overlap-add
        music_fft.synthesize(&masked_spectrum, &mut masked_music_accum, pos);
        voice_fft.synthesize(&voice_spectrum, &mut voice_accum, pos);

        pos += hop_size;
    }

    // Mix
    let music_gain = db_to_gain(config.output.music_gain_db);
    let voice_gain = db_to_gain(config.output.voice_gain_db);

    let mut output = vec![0.0f32; total_len];
    mix_frame(
        &masked_music_accum[..total_len],
        &voice_accum[..total_len],
        music_gain,
        voice_gain,
        &mut output,
    );

    // Write output
    write_wav(output_path, &output, sample_rate)?;

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
