use rustfft::num_complex::Complex;

pub struct VoiceGate {
    gate_open: bool,
    envelope: f32,
    threshold_on_db: f32,
    threshold_off_db: f32,
    alpha_attack: f32,
    alpha_release: f32,
}

impl VoiceGate {
    pub fn new(
        threshold_on_db: f32,
        threshold_off_db: f32,
        attack_ms: f32,
        release_ms: f32,
        sample_rate: u32,
        hop_size: usize,
    ) -> Self {
        // Alpha from time constant: alpha = exp(-1 / (time_ms * sample_rate / (1000 * hop_size)))
        let hop_rate = sample_rate as f32 / hop_size as f32;
        let alpha_attack = (-1.0 / (attack_ms * 0.001 * hop_rate)).exp();
        let alpha_release = (-1.0 / (release_ms * 0.001 * hop_rate)).exp();

        Self {
            gate_open: false,
            envelope: 0.0,
            threshold_on_db,
            threshold_off_db,
            alpha_attack,
            alpha_release,
        }
    }

    /// Update gate state from voice spectrum. Returns smoothed envelope (0.0..1.0).
    pub fn update(&mut self, voice_spectrum: &[Complex<f32>]) -> f32 {
        let rms = spectrum_rms(voice_spectrum);
        let db = 20.0 * (rms + 1e-10).log10();

        // Hysteresis gate
        if self.gate_open {
            if db < self.threshold_off_db {
                self.gate_open = false;
            }
        } else if db > self.threshold_on_db {
            self.gate_open = true;
        }

        let target = if self.gate_open { 1.0 } else { 0.0 };

        // Asymmetric smoothing: fast attack, slow release
        let alpha = if target > self.envelope {
            self.alpha_attack
        } else {
            self.alpha_release
        };
        self.envelope = alpha * self.envelope + (1.0 - alpha) * target;

        self.envelope
    }
}

fn spectrum_rms(spectrum: &[Complex<f32>]) -> f32 {
    if spectrum.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = spectrum.iter().map(|c| c.norm_sqr()).sum();
    (sum_sq / spectrum.len() as f32).sqrt()
}
