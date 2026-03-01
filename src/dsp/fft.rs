use realfft::{RealFftPlanner, RealToComplex, ComplexToReal};
use rustfft::num_complex::Complex;
use std::sync::Arc;

pub struct OverlapAddProcessor {
    fft_size: usize,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    fft_in: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    ifft_out: Vec<f32>,
    scratch_forward: Vec<Complex<f32>>,
    scratch_inverse: Vec<Complex<f32>>,
}

impl OverlapAddProcessor {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(fft_size);
        let inverse = planner.plan_fft_inverse(fft_size);

        let scratch_forward = forward.make_scratch_vec();
        let scratch_inverse = inverse.make_scratch_vec();

        let window = hann_window(fft_size);

        Self {
            fft_size,
            forward,
            inverse,
            window,
            fft_in: vec![0.0; fft_size],
            spectrum: vec![Complex::new(0.0, 0.0); fft_size / 2 + 1],
            ifft_out: vec![0.0; fft_size],
            scratch_forward,
            scratch_inverse,
        }
    }

    pub fn spectrum_len(&self) -> usize {
        self.fft_size / 2 + 1
    }

    /// Apply window and forward FFT. Returns the spectrum.
    pub fn process_frame(&mut self, input_frame: &[f32]) -> &[Complex<f32>] {
        debug_assert_eq!(input_frame.len(), self.fft_size);

        // Apply Hann window
        for (i, sample) in input_frame.iter().enumerate() {
            self.fft_in[i] = sample * self.window[i];
        }

        // Forward FFT (r2c)
        self.forward
            .process_with_scratch(&mut self.fft_in, &mut self.spectrum, &mut self.scratch_forward)
            .expect("FFT forward failed");

        &self.spectrum
    }

    /// Inverse FFT + normalize + overlap-add into the output accumulator.
    pub fn synthesize(
        &mut self,
        spectrum: &[Complex<f32>],
        output_accum: &mut [f32],
        position: usize,
    ) {
        debug_assert_eq!(spectrum.len(), self.spectrum_len());

        // Copy spectrum into our buffer for in-place IFFT
        self.spectrum.copy_from_slice(spectrum);

        // Inverse FFT (c2r)
        self.inverse
            .process_with_scratch(&mut self.spectrum, &mut self.ifft_out, &mut self.scratch_inverse)
            .expect("FFT inverse failed");

        // Normalize by fft_size (realfft convention) and apply synthesis window,
        // then overlap-add
        let norm = 1.0 / self.fft_size as f32;
        for i in 0..self.fft_size {
            let out_idx = position + i;
            if out_idx < output_accum.len() {
                output_accum[out_idx] += self.ifft_out[i] * norm * self.window[i];
            }
        }
    }
}

/// Hann window: w[n] = 0.5 * (1 - cos(2*pi*n / (N-1)))
/// Satisfies COLA (constant overlap-add) with 50% overlap.
fn hann_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|n| {
            let phase = 2.0 * std::f32::consts::PI * n as f32 / (size - 1) as f32;
            0.5 * (1.0 - phase.cos())
        })
        .collect()
}
