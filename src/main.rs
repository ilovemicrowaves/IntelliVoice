mod audio;
mod config;
mod dsp;
#[cfg(feature = "gui")]
mod gui;
#[cfg(feature = "pipewire")]
mod pipewire_filter;
mod realtime;

use clap::{Parser, Subcommand};
use config::Config;
use dsp::{DspPipeline, mix_frame, mixer::db_to_gain};
use std::path::{Path, PathBuf};

type WavResult = Result<(Vec<f32>, Vec<f32>, u32), Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "spectralblend", about = "Spectral voice masking audio processor")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
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
    /// List available audio input/output devices
    Devices,
    /// Process audio in real-time from input devices to output
    Realtime {
        /// Music input device name (substring match)
        #[arg(long)]
        music: String,
        /// Voice input device name (substring match)
        #[arg(long)]
        voice: String,
        /// Output device name (substring match, defaults to system default)
        #[arg(long)]
        output: Option<String>,
        /// Path to config TOML file (uses defaults if omitted)
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run as a PipeWire filter node (route apps via pw-link)
    #[cfg(feature = "pipewire")]
    Pipewire {
        /// Path to config TOML file (uses defaults if omitted)
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Launch the GUI for routing audio apps through SpectralBlend
    #[cfg(feature = "gui")]
    Gui {
        /// Path to config TOML file (uses defaults if omitted)
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            #[cfg(feature = "gui")]
            {
                Command::Gui { config: None }
            }
            #[cfg(not(feature = "gui"))]
            {
                // Re-parse with help flag to print usage
                use clap::CommandFactory;
                Cli::command().print_help().ok();
                println!();
                std::process::exit(0);
            }
        }
    };

    match command {
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
        Command::Devices => {
            let host = audio::default_host();
            audio::list_devices(&host);
        }
        Command::Realtime {
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

            let host = audio::default_host();
            let music_device = audio::find_input_device(&host, &music).unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1);
            });
            let voice_device = audio::find_input_device(&host, &voice).unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1);
            });
            let output_device = match output {
                Some(name) => audio::find_output_device(&host, &name).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    std::process::exit(1);
                }),
                None => {
                    use cpal::traits::HostTrait;
                    host.default_output_device().unwrap_or_else(|| {
                        eprintln!("No default output device available");
                        std::process::exit(1);
                    })
                }
            };

            // Validate device configs
            let sr = cfg.processing.sample_rate;
            audio::validate_device_config(&music_device, sr, true).unwrap_or_else(|e| {
                eprintln!("Music device: {e}");
                std::process::exit(1);
            });
            audio::validate_device_config(&voice_device, sr, true).unwrap_or_else(|e| {
                eprintln!("Voice device: {e}");
                std::process::exit(1);
            });
            audio::validate_device_config(&output_device, sr, false).unwrap_or_else(|e| {
                eprintln!("Output device: {e}");
                std::process::exit(1);
            });

            if let Err(e) = realtime::run_realtime(music_device, voice_device, output_device, &cfg)
            {
                eprintln!("Real-time processing failed: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(feature = "pipewire")]
        Command::Pipewire { config } => {
            let cfg = match config {
                Some(path) => Config::load(&path).unwrap_or_else(|e| {
                    eprintln!("Failed to load config: {e}");
                    std::process::exit(1);
                }),
                None => Config::default(),
            };
            if let Err(e) = pipewire_filter::run_pipewire(&cfg) {
                eprintln!("PipeWire filter failed: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(feature = "gui")]
        Command::Gui { config } => {
            let cfg = match config {
                Some(path) => Config::load(&path).unwrap_or_else(|e| {
                    eprintln!("Failed to load config: {e}");
                    std::process::exit(1);
                }),
                None => Config::default(),
            };
            if let Err(e) = gui::run_gui(&cfg) {
                eprintln!("GUI failed: {e}");
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

    // Pipeline owns all processors and pre-allocated work buffers
    let mut pipeline = DspPipeline::new(config);

    // Output accumulators (overlap-add targets), one per channel per stream
    let output_len = total_len + fft_size;
    let mut masked_music_accum_l = vec![0.0f32; output_len];
    let mut masked_music_accum_r = vec![0.0f32; output_len];
    let mut voice_accum_l = vec![0.0f32; output_len];
    let mut voice_accum_r = vec![0.0f32; output_len];

    // Processing loop
    let mut pos = 0;
    while pos + fft_size <= total_len {
        pipeline.process_frame(
            &music_l[pos..pos + fft_size],
            &music_r[pos..pos + fft_size],
            &voice_l[pos..pos + fft_size],
            &voice_r[pos..pos + fft_size],
            &config.masking,
            &mut masked_music_accum_l,
            &mut masked_music_accum_r,
            &mut voice_accum_l,
            &mut voice_accum_r,
            pos,
            1.0, // full compression
        );
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
    let avg_mask = if pipeline.frame_count > 0 {
        pipeline.mask_sum / pipeline.frame_count as f64
    } else {
        1.0
    };
    eprintln!("  Frames processed: {}", pipeline.frame_count);
    eprintln!("  Average mask value: {avg_mask:.4}");
    eprintln!("  Min mask value: {:.4} ({:.1} dB)", pipeline.mask_min, 20.0 * pipeline.mask_min.max(1e-10).log10());
    eprintln!("  Output written to: {}", output_path.display());

    Ok(())
}
