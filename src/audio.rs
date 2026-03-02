use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host, StreamConfig, SampleFormat, SupportedStreamConfigRange};

/// Get the default CPAL host.
pub fn default_host() -> Host {
    cpal::default_host()
}

/// List all available input and output devices with their supported configs.
pub fn list_devices(host: &Host) {
    eprintln!("Audio host: {:?}", host.id());
    eprintln!();

    eprintln!("=== Input Devices ===");
    match host.input_devices() {
        Ok(devices) => {
            for device in devices {
                print_device_info(&device, true);
            }
        }
        Err(e) => eprintln!("  Error listing input devices: {e}"),
    }

    eprintln!();
    eprintln!("=== Output Devices ===");
    match host.output_devices() {
        Ok(devices) => {
            for device in devices {
                print_device_info(&device, false);
            }
        }
        Err(e) => eprintln!("  Error listing output devices: {e}"),
    }

    // Print defaults
    eprintln!();
    if let Some(d) = host.default_input_device() {
        eprintln!("Default input:  {}", d.name().unwrap_or_default());
    }
    if let Some(d) = host.default_output_device() {
        eprintln!("Default output: {}", d.name().unwrap_or_default());
    }
}

fn print_device_info(device: &Device, is_input: bool) {
    let name = device.name().unwrap_or_else(|_| "<unknown>".into());
    eprintln!("  {name}");

    let configs: Box<dyn Iterator<Item = SupportedStreamConfigRange>> = if is_input {
        match device.supported_input_configs() {
            Ok(c) => Box::new(c),
            Err(e) => {
                eprintln!("    (error reading configs: {e})");
                return;
            }
        }
    } else {
        match device.supported_output_configs() {
            Ok(c) => Box::new(c),
            Err(e) => {
                eprintln!("    (error reading configs: {e})");
                return;
            }
        }
    };

    for cfg in configs {
        eprintln!(
            "    ch={} rate={}–{} fmt={:?}",
            cfg.channels(),
            cfg.min_sample_rate().0,
            cfg.max_sample_rate().0,
            cfg.sample_format(),
        );
    }
}

/// Find an input device whose name contains the given pattern (case-insensitive).
pub fn find_input_device(host: &Host, name_pattern: &str) -> Result<Device, String> {
    let pattern = name_pattern.to_lowercase();
    let devices = host.input_devices().map_err(|e| format!("Cannot list input devices: {e}"))?;
    for device in devices {
        if let Ok(name) = device.name() {
            if name.to_lowercase().contains(&pattern) {
                return Ok(device);
            }
        }
    }
    Err(format!("No input device matching \"{name_pattern}\""))
}

/// Find an output device whose name contains the given pattern (case-insensitive).
pub fn find_output_device(host: &Host, name_pattern: &str) -> Result<Device, String> {
    let pattern = name_pattern.to_lowercase();
    let devices = host.output_devices().map_err(|e| format!("Cannot list output devices: {e}"))?;
    for device in devices {
        if let Ok(name) = device.name() {
            if name.to_lowercase().contains(&pattern) {
                return Ok(device);
            }
        }
    }
    Err(format!("No output device matching \"{name_pattern}\""))
}

/// Build a stereo f32 stream config at the given sample rate.
pub fn build_stream_config(sample_rate: u32) -> StreamConfig {
    StreamConfig {
        channels: 2,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    }
}

/// Check that a device supports stereo f32 at the given sample rate.
/// Returns Ok(()) or an error describing the mismatch.
pub fn validate_device_config(
    device: &Device,
    sample_rate: u32,
    is_input: bool,
) -> Result<(), String> {
    let configs: Vec<SupportedStreamConfigRange> = if is_input {
        device
            .supported_input_configs()
            .map_err(|e| format!("Cannot query device configs: {e}"))?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|e| format!("Cannot query device configs: {e}"))?
            .collect()
    };

    let sr = cpal::SampleRate(sample_rate);
    for cfg in &configs {
        if cfg.channels() >= 2
            && cfg.sample_format() == SampleFormat::F32
            && cfg.min_sample_rate() <= sr
            && cfg.max_sample_rate() >= sr
        {
            return Ok(());
        }
    }

    Err(format!(
        "Device does not support stereo f32 at {sample_rate} Hz. Supported configs: {:?}",
        configs
            .iter()
            .map(|c| format!(
                "ch={} rate={}–{} fmt={:?}",
                c.channels(),
                c.min_sample_rate().0,
                c.max_sample_rate().0,
                c.sample_format()
            ))
            .collect::<Vec<_>>()
    ))
}
