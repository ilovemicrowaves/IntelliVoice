use crate::config::MaskingConfig;

pub struct SpectralMask {
    smoothed_mask: Vec<f32>,
    spectrum_len: usize,
    fft_size: usize,
    sample_rate: u32,
    alpha_attack: f32,
    alpha_release: f32,
}

impl SpectralMask {
    pub fn new(fft_size: usize, sample_rate: u32) -> Self {
        let spectrum_len = fft_size / 2 + 1;
        Self {
            smoothed_mask: vec![1.0; spectrum_len],
            spectrum_len,
            fft_size,
            sample_rate,
            // Temporal smoothing: fast attack, slow release
            alpha_attack: 0.3,
            alpha_release: 0.85,
        }
    }

    /// Frequency of FFT bin k.
    fn bin_freq(&self, k: usize) -> f32 {
        k as f32 * self.sample_rate as f32 / self.fft_size as f32
    }

    /// Build the spectral mask from voice and music magnitudes.
    ///
    /// Returns a Vec<f32> of length spectrum_len where each value is in [floor, 1.0].
    /// 1.0 = music untouched, floor = maximum reduction.
    pub fn build_mask(
        &mut self,
        voice_mag: &[f32],
        music_mag: &[f32],
        gate_value: f32,
        config: &MaskingConfig,
    ) -> Vec<f32> {
        debug_assert_eq!(voice_mag.len(), self.spectrum_len);
        debug_assert_eq!(music_mag.len(), self.spectrum_len);

        let floor = db_to_linear(config.max_reduction_db);
        let max_reduction = 1.0 - floor;

        // Step 1-2: Compute raw mask from voice/music ratio
        let mut mask: Vec<f32> = (0..self.spectrum_len)
            .map(|k| {
                let ratio = voice_mag[k] / (music_mag[k] + 1e-10);
                let scaled = (ratio * config.sensitivity).clamp(0.0, max_reduction);
                1.0 - config.depth * scaled
            })
            .collect();

        // Step 3: Sub-bass protection
        for (k, m) in mask.iter_mut().enumerate() {
            if self.bin_freq(k) < config.sub_bass_protect_hz {
                *m = 1.0;
            }
        }

        // Step 4: Frequency focus taper — outside focus range, interpolate toward 1.0
        for (k, m) in mask.iter_mut().enumerate() {
            let freq = self.bin_freq(k);
            let weight = focus_weight(freq, config.focus_low_hz, config.focus_high_hz);
            *m = lerp(1.0, *m, weight);
        }

        // Step 5: Spectral smoothing (moving average across bins)
        if config.spectral_smooth_bins > 1 {
            mask = spectral_smooth(&mask, config.spectral_smooth_bins);
        }

        // Step 6: Gate modulation — when gate is closed, mask is 1.0 everywhere
        for v in &mut mask {
            *v = lerp(1.0, *v, gate_value);
        }

        // Step 7: Floor enforcement
        for v in &mut mask {
            *v = v.max(floor);
        }

        // Step 8: Temporal smoothing (asymmetric)
        for (sm, &m) in self.smoothed_mask.iter_mut().zip(mask.iter()) {
            let alpha = if m < *sm {
                self.alpha_attack // mask decreasing = attack
            } else {
                self.alpha_release // mask increasing = release
            };
            *sm = alpha * *sm + (1.0 - alpha) * m;
        }

        self.smoothed_mask.clone()
    }
}

/// Linear interpolation: lerp(a, b, t) = a + t * (b - a)
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + t * (b - a)
}

/// Convert dB to linear gain: 10^(db/20)
fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Bandpass focus weight: 1.0 inside [low, high], tapers to 0.0 outside.
/// Uses a smooth transition over one octave on each side.
fn focus_weight(freq: f32, low_hz: f32, high_hz: f32) -> f32 {
    if freq <= 0.0 {
        return 0.0;
    }
    if freq >= low_hz && freq <= high_hz {
        return 1.0;
    }
    if freq < low_hz {
        // Taper: one octave below low_hz
        let edge = low_hz / 2.0;
        if freq <= edge {
            return 0.0;
        }
        return (freq - edge) / (low_hz - edge);
    }
    // freq > high_hz: taper one octave above
    let edge = high_hz * 2.0;
    if freq >= edge {
        return 0.0;
    }
    (edge - freq) / (edge - high_hz)
}

/// Moving average across bins.
fn spectral_smooth(mask: &[f32], width: usize) -> Vec<f32> {
    let half = width / 2;
    let len = mask.len();
    (0..len)
        .map(|k| {
            let start = k.saturating_sub(half);
            let end = (k + half + 1).min(len);
            let sum: f32 = mask[start..end].iter().sum();
            sum / (end - start) as f32
        })
        .collect()
}
