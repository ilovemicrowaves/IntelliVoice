/// Gentle voice compressor using a soft ratio curve.
///
/// Instead of hard-targeting a fixed RMS (which sounds aggressive), this uses
/// a dB-domain ratio: gain_db = (target_db - input_db) / ratio. A ratio of 3
/// means a voice 12dB below target gets boosted by only 4dB — natural-sounding.
pub struct VoiceCompressor {
    target_db: f32,
    noise_floor: f32,
    ratio: f32,
    smoothed_gain: f32,
    attack_alpha: f32,
    release_alpha: f32,
    max_gain: f32,
}

impl VoiceCompressor {
    /// `target_rms` — desired output RMS for voice at nominal level.
    /// `noise_floor` — frames below this RMS are considered silence.
    /// `ratio` — compression softness (higher = gentler). 2.0 = moderate, 4.0 = very gentle.
    pub fn new(
        target_rms: f32,
        noise_floor: f32,
        ratio: f32,
        sample_rate: u32,
        hop_size: usize,
    ) -> Self {
        let hop_rate = sample_rate as f32 / hop_size as f32;
        Self {
            target_db: 20.0 * target_rms.log10(),
            noise_floor,
            ratio,
            smoothed_gain: 1.0,
            // Slow attack/release for smooth, natural feel
            attack_alpha: (-1.0 / (30.0 * 0.001 * hop_rate)).exp(),
            release_alpha: (-1.0 / (200.0 * 0.001 * hop_rate)).exp(),
            max_gain: 8.0,
        }
    }

    pub fn process(&mut self, frame: &[f32]) -> Vec<f32> {
        let rms = frame_rms(frame);

        let target_gain = if rms > self.noise_floor {
            let input_db = 20.0 * rms.log10();
            let diff_db = self.target_db - input_db;
            // Only boost (don't attenuate loud voice — let it be natural)
            let gain_db = if diff_db > 0.0 {
                diff_db / self.ratio
            } else {
                0.0
            };
            10.0_f32.powf(gain_db / 20.0).min(self.max_gain)
        } else {
            1.0
        };

        // Smooth gain changes
        let alpha = if target_gain < self.smoothed_gain {
            self.attack_alpha
        } else {
            self.release_alpha
        };
        self.smoothed_gain = alpha * self.smoothed_gain + (1.0 - alpha) * target_gain;

        frame.iter().map(|&s| s * self.smoothed_gain).collect()
    }
}

fn frame_rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = frame.iter().map(|s| s * s).sum();
    (sum_sq / frame.len() as f32).sqrt()
}
