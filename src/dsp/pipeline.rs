use rustfft::num_complex::Complex;

use crate::config::{Config, MaskingConfig};
use super::{OverlapAddProcessor, SpectralMask, VoiceCompressor, VoiceGate};

/// Pre-allocated DSP pipeline owning all processors and work buffers.
///
/// Processes one FFT frame of stereo music + stereo voice through the full
/// compress → FFT → gate → mask → IFFT → overlap-add chain.
pub struct DspPipeline {
    // Per-channel FFT processors
    music_fft_l: OverlapAddProcessor,
    music_fft_r: OverlapAddProcessor,
    voice_fft_l: OverlapAddProcessor,
    voice_fft_r: OverlapAddProcessor,

    // Per-channel spectral masks and voice compressors
    spectral_mask_l: SpectralMask,
    spectral_mask_r: SpectralMask,
    voice_compressor_l: VoiceCompressor,
    voice_compressor_r: VoiceCompressor,

    // Shared voice gate
    voice_gate: VoiceGate,

    // Pre-allocated work buffers
    compressed_voice: Vec<f32>,       // fft_size
    music_spectrum_l: Vec<Complex<f32>>,  // spectrum_len
    music_spectrum_r: Vec<Complex<f32>>,
    voice_spectrum_l: Vec<Complex<f32>>,
    voice_spectrum_r: Vec<Complex<f32>>,
    music_mag: Vec<f32>,              // spectrum_len
    voice_mag: Vec<f32>,              // spectrum_len
    mask: Vec<f32>,                   // spectrum_len
    masked_spectrum: Vec<Complex<f32>>, // spectrum_len

    // Tracking stats
    pub frame_count: u64,
    pub mask_sum: f64,
    pub mask_min: f32,
}

impl DspPipeline {
    pub fn new(config: &Config) -> Self {
        let fft_size = config.processing.fft_size;
        let hop_size = config.hop_size();
        let sample_rate = config.processing.sample_rate;
        let spectrum_len = fft_size / 2 + 1;

        Self {
            music_fft_l: OverlapAddProcessor::new(fft_size),
            music_fft_r: OverlapAddProcessor::new(fft_size),
            voice_fft_l: OverlapAddProcessor::new(fft_size),
            voice_fft_r: OverlapAddProcessor::new(fft_size),

            spectral_mask_l: SpectralMask::new(fft_size, sample_rate),
            spectral_mask_r: SpectralMask::new(fft_size, sample_rate),
            voice_compressor_l: VoiceCompressor::new(0.15, 0.002, 1.8, sample_rate, hop_size),
            voice_compressor_r: VoiceCompressor::new(0.15, 0.002, 1.8, sample_rate, hop_size),

            voice_gate: VoiceGate::new(
                config.envelope.gate_threshold_on,
                config.envelope.gate_threshold_off,
                config.envelope.attack_ms,
                config.envelope.release_ms,
                sample_rate,
                hop_size,
            ),

            compressed_voice: vec![0.0; fft_size],
            music_spectrum_l: vec![Complex::new(0.0, 0.0); spectrum_len],
            music_spectrum_r: vec![Complex::new(0.0, 0.0); spectrum_len],
            voice_spectrum_l: vec![Complex::new(0.0, 0.0); spectrum_len],
            voice_spectrum_r: vec![Complex::new(0.0, 0.0); spectrum_len],
            music_mag: vec![0.0; spectrum_len],
            voice_mag: vec![0.0; spectrum_len],
            mask: vec![0.0; spectrum_len],
            masked_spectrum: vec![Complex::new(0.0, 0.0); spectrum_len],

            frame_count: 0,
            mask_sum: 0.0,
            mask_min: 1.0,
        }
    }

    /// Process one FFT frame through the full pipeline.
    ///
    /// Reads fft_size samples from each input slice, runs compress → FFT → gate → mask → IFFT,
    /// and overlap-adds into the accumulator slices at `position`.
    #[allow(clippy::too_many_arguments)]
    pub fn process_frame(
        &mut self,
        music_l: &[f32],
        music_r: &[f32],
        voice_l: &[f32],
        voice_r: &[f32],
        masking_config: &MaskingConfig,
        music_accum_l: &mut [f32],
        music_accum_r: &mut [f32],
        voice_accum_l: &mut [f32],
        voice_accum_r: &mut [f32],
        position: usize,
    ) {
        // Compress voice L/R independently
        self.voice_compressor_l.process(voice_l, &mut self.compressed_voice);
        self.voice_spectrum_l.copy_from_slice(self.voice_fft_l.process_frame(&self.compressed_voice));

        self.voice_compressor_r.process(voice_r, &mut self.compressed_voice);
        self.voice_spectrum_r.copy_from_slice(self.voice_fft_r.process_frame(&self.compressed_voice));

        // Forward FFT music
        self.music_spectrum_l.copy_from_slice(self.music_fft_l.process_frame(music_l));
        self.music_spectrum_r.copy_from_slice(self.music_fft_r.process_frame(music_r));

        // Shared voice gate from both channels
        let gate_value = self.voice_gate.update_stereo(&self.voice_spectrum_l, &self.voice_spectrum_r);

        // Build mask and apply — LEFT channel
        for (i, c) in self.music_spectrum_l.iter().enumerate() {
            self.music_mag[i] = c.norm();
        }
        for (i, c) in self.voice_spectrum_l.iter().enumerate() {
            self.voice_mag[i] = c.norm();
        }
        self.spectral_mask_l.build_mask(&self.voice_mag, &self.music_mag, gate_value, masking_config, &mut self.mask);

        for (i, (bin, &m)) in self.music_spectrum_l.iter().zip(self.mask.iter()).enumerate() {
            self.masked_spectrum[i] = bin * m;
        }
        self.music_fft_l.synthesize(&self.masked_spectrum, music_accum_l, position);

        // Track stats (left channel)
        let avg_l: f32 = self.mask.iter().sum::<f32>() / self.mask.len() as f32;
        let min_l = self.mask.iter().copied().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(1.0);

        // Build mask and apply — RIGHT channel
        for (i, c) in self.music_spectrum_r.iter().enumerate() {
            self.music_mag[i] = c.norm();
        }
        for (i, c) in self.voice_spectrum_r.iter().enumerate() {
            self.voice_mag[i] = c.norm();
        }
        self.spectral_mask_r.build_mask(&self.voice_mag, &self.music_mag, gate_value, masking_config, &mut self.mask);

        for (i, (bin, &m)) in self.music_spectrum_r.iter().zip(self.mask.iter()).enumerate() {
            self.masked_spectrum[i] = bin * m;
        }
        self.music_fft_r.synthesize(&self.masked_spectrum, music_accum_r, position);

        // Track stats (right channel, combine with left)
        let avg_r: f32 = self.mask.iter().sum::<f32>() / self.mask.len() as f32;
        let min_r = self.mask.iter().copied().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(1.0);

        self.mask_sum += ((avg_l + avg_r) * 0.5) as f64;
        self.mask_min = self.mask_min.min(min_l).min(min_r);
        self.frame_count += 1;

        // IFFT + overlap-add voice (unmasked, for mixing)
        self.voice_fft_l.synthesize(&self.voice_spectrum_l, voice_accum_l, position);
        self.voice_fft_r.synthesize(&self.voice_spectrum_r, voice_accum_r, position);
    }
}
