use rustfft::num_complex::Complex;

pub struct VoiceGate {
    gate_open: bool,
    envelope: f32,
    alpha_attack: f32,
    alpha_release: f32,
    // Adaptive noise floor
    noise_floor_db: f32,
    margin_db: f32,      // Voice must be this far above noise floor to open
    hysteresis_db: f32,   // Gate closes this many dB below the open threshold
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
        let hop_rate = sample_rate as f32 / hop_size as f32;
        let alpha_attack = (-1.0 / (attack_ms * 0.001 * hop_rate)).exp();
        let alpha_release = (-1.0 / (release_ms * 0.001 * hop_rate)).exp();

        // Derive margin from the config threshold gap (default: -40 - (-50) = 10 dB)
        let margin_db = (threshold_on_db - threshold_off_db).max(4.0);

        Self {
            gate_open: false,
            envelope: 0.0,
            alpha_attack,
            alpha_release,
            noise_floor_db: -80.0,
            margin_db,
            hysteresis_db: margin_db * 0.6,
        }
    }

    /// Update gate state from a single voice spectrum. Returns smoothed envelope (0.0..1.0).
    #[allow(dead_code)]
    pub fn update(&mut self, voice_spectrum: &[Complex<f32>]) -> f32 {
        let rms = spectrum_rms(voice_spectrum);
        let db = 20.0 * (rms + 1e-10).log10();
        self.update_from_db(db)
    }

    /// Update gate state from stereo voice spectra. Uses the louder channel's RMS
    /// so both sides gate together, avoiding disorienting one-sided masking.
    pub fn update_stereo(
        &mut self,
        voice_spectrum_l: &[Complex<f32>],
        voice_spectrum_r: &[Complex<f32>],
    ) -> f32 {
        let rms_l = spectrum_rms(voice_spectrum_l);
        let rms_r = spectrum_rms(voice_spectrum_r);
        let rms = rms_l.max(rms_r);
        let db = 20.0 * (rms + 1e-10).log10();
        self.update_from_db(db)
    }

    fn update_from_db(&mut self, db: f32) -> f32 {
        // Adaptive noise floor tracking:
        // - Falls quickly when signal drops (catches new ambient level in ~0.3s)
        // - Rises very slowly when signal is above (doesn't chase speech, ~10s to adapt)
        if db < self.noise_floor_db + 3.0 {
            // Near or below current estimate: fast downward adaptation
            self.noise_floor_db = 0.85 * self.noise_floor_db + 0.15 * db;
        } else {
            // Above noise floor: very slow upward drift
            self.noise_floor_db = 0.998 * self.noise_floor_db + 0.002 * db;
        }
        self.noise_floor_db = self.noise_floor_db.clamp(-80.0, -10.0);

        // Dynamic thresholds relative to the noise floor
        let threshold_on = self.noise_floor_db + self.margin_db;
        let threshold_off = self.noise_floor_db + self.margin_db - self.hysteresis_db;

        // Hysteresis gate
        if self.gate_open {
            if db < threshold_off {
                self.gate_open = false;
            }
        } else if db > threshold_on {
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
