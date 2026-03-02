use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub processing: ProcessingConfig,
    pub masking: MaskingConfig,
    pub envelope: EnvelopeConfig,
    pub output: OutputConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProcessingConfig {
    pub fft_size: usize,
    pub hop_ratio: f32,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MaskingConfig {
    pub depth: f32,
    pub sensitivity: f32,
    pub max_reduction_db: f32,
    pub sub_bass_protect_hz: f32,
    pub focus_low_hz: f32,
    pub focus_high_hz: f32,
    pub spectral_smooth_bins: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EnvelopeConfig {
    pub attack_ms: f32,
    pub release_ms: f32,
    pub gate_threshold_on: f32,
    pub gate_threshold_off: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    pub voice_gain_db: f32,
    pub music_gain_db: f32,
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            fft_size: 2048,
            hop_ratio: 0.5,
            sample_rate: 48000,
        }
    }
}

impl Default for MaskingConfig {
    fn default() -> Self {
        Self {
            depth: 0.6,
            sensitivity: 1.5,
            max_reduction_db: -6.0,
            sub_bass_protect_hz: 80.0,
            focus_low_hz: 300.0,
            focus_high_hz: 6000.0,
            spectral_smooth_bins: 3,
        }
    }
}

impl Default for EnvelopeConfig {
    fn default() -> Self {
        Self {
            attack_ms: 5.0,
            release_ms: 100.0,
            gate_threshold_on: -40.0,
            gate_threshold_off: -50.0,
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            voice_gain_db: 0.0,
            music_gain_db: 0.0,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.processing.fft_size.is_power_of_two() {
            return Err(format!(
                "fft_size must be a power of 2, got {}",
                self.processing.fft_size
            ));
        }
        if self.processing.hop_ratio <= 0.0 || self.processing.hop_ratio > 1.0 {
            return Err(format!(
                "hop_ratio must be in (0.0, 1.0], got {}",
                self.processing.hop_ratio
            ));
        }
        if self.masking.depth < 0.0 || self.masking.depth > 1.0 {
            return Err(format!(
                "masking depth must be in [0.0, 1.0], got {}",
                self.masking.depth
            ));
        }
        if self.masking.max_reduction_db > 0.0 {
            return Err(format!(
                "max_reduction_db must be <= 0.0, got {}",
                self.masking.max_reduction_db
            ));
        }
        Ok(())
    }

    pub fn hop_size(&self) -> usize {
        (self.processing.fft_size as f32 * self.processing.hop_ratio) as usize
    }

}
