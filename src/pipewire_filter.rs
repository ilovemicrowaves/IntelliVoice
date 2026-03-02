//! PipeWire filter node for SpectralBlend.
//!
//! Creates a DSP filter node with 4 input ports (music L/R, voice L/R)
//! and 2 output ports (mix L/R). Apps can be routed to the appropriate
//! ports via `pw-link`, `qpwgraph`, or WirePlumber rules.
//!
//! Uses the safe `pipewire` crate for MainLoop / signal handling and raw
//! `pw_filter_*` FFI for the filter node (no safe Filter wrapper exists).

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use pipewire::main_loop::MainLoopBox;
use pipewire::properties::properties;
use pipewire::sys as pw_sys;

use crate::config::Config;
use crate::dsp::mixer::db_to_gain;
use crate::dsp::DspPipeline;

// ---------------------------------------------------------------------------
// Runtime parameters (lock-free GUI → RT thread communication)
// ---------------------------------------------------------------------------

/// Lock-free parameters shared between the GUI thread and the RT audio thread.
/// Uses `AtomicU32` with `f32::to_bits()`/`from_bits()` for safe, allocation-free
/// parameter passing.
#[allow(dead_code)]
pub struct RuntimeParams {
    compression_mix: AtomicU32, // f32 [0.0, 1.0]
    voice_gain: AtomicU32,     // f32, linear
    masking_depth: AtomicU32,  // f32 [0.0, 1.0]
    wideband: AtomicU32,       // f32 [0.0, 1.0]
}

#[allow(dead_code)]
impl RuntimeParams {
    pub fn new(config: &Config) -> Self {
        Self {
            compression_mix: AtomicU32::new(1.0_f32.to_bits()),
            voice_gain: AtomicU32::new(db_to_gain(config.output.voice_gain_db).to_bits()),
            masking_depth: AtomicU32::new(config.masking.depth.to_bits()),
            wideband: AtomicU32::new(config.masking.focus_strength.to_bits()),
        }
    }

    pub fn load_compression_mix(&self) -> f32 {
        f32::from_bits(self.compression_mix.load(Ordering::Relaxed))
    }
    pub fn store_compression_mix(&self, v: f32) {
        self.compression_mix.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn load_voice_gain(&self) -> f32 {
        f32::from_bits(self.voice_gain.load(Ordering::Relaxed))
    }
    pub fn store_voice_gain(&self, v: f32) {
        self.voice_gain.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn load_masking_depth(&self) -> f32 {
        f32::from_bits(self.masking_depth.load(Ordering::Relaxed))
    }
    pub fn store_masking_depth(&self, v: f32) {
        self.masking_depth.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn load_wideband(&self) -> f32 {
        f32::from_bits(self.wideband.load(Ordering::Relaxed))
    }
    pub fn store_wideband(&self, v: f32) {
        self.wideband.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// Maximum quantum size we support. PipeWire default max is 8192.
pub(crate) const MAX_QUANTUM: usize = 8192;

/// Static silence buffer for null (disconnected) input ports.
/// Avoids allocation in the RT callback.
pub(crate) static SILENCE: [f32; MAX_QUANTUM] = [0.0; MAX_QUANTUM];

// ---------------------------------------------------------------------------
// Processing state
// ---------------------------------------------------------------------------

/// All state for the PipeWire filter node's real-time callback.
/// Pre-allocated at startup — zero allocations during processing.
#[allow(dead_code)]
pub(crate) struct FilterState {
    // Port handles (opaque pointers for pw_filter_get_dsp_buffer)
    pub(crate) music_l_port: *mut c_void,
    pub(crate) music_r_port: *mut c_void,
    pub(crate) voice_l_port: *mut c_void,
    pub(crate) voice_r_port: *mut c_void,
    pub(crate) output_l_port: *mut c_void,
    pub(crate) output_r_port: *mut c_void,

    // DSP
    pipeline: DspPipeline,
    config: Config,
    fft_size: usize,
    hop_size: usize,
    music_gain: f32,
    voice_gain: f32,
    pub(crate) runtime_params: Arc<RuntimeParams>,

    // Sliding input windows (fft_size samples each)
    music_window_l: Vec<f32>,
    music_window_r: Vec<f32>,
    voice_window_l: Vec<f32>,
    voice_window_r: Vec<f32>,

    // Overlap-add accumulators (fft_size samples each)
    music_accum_l: Vec<f32>,
    music_accum_r: Vec<f32>,
    voice_accum_l: Vec<f32>,
    voice_accum_r: Vec<f32>,

    // Output FIFO (pre-allocated, linear drain)
    output_l: Vec<f32>,
    output_r: Vec<f32>,
    output_len: usize,

    // Accumulation counter for variable-quantum handling
    samples_since_last_hop: usize,

    // Debug recording (captures filter I/O when enabled)
    pub(crate) debug_music_l: Vec<f32>,
    pub(crate) debug_music_r: Vec<f32>,
    pub(crate) debug_voice_l: Vec<f32>,
    pub(crate) debug_voice_r: Vec<f32>,
    pub(crate) debug_output_l: Vec<f32>,
    pub(crate) debug_output_r: Vec<f32>,
    pub(crate) debug_recording: bool,
}

impl FilterState {
    pub(crate) fn new(config: &Config, runtime_params: Arc<RuntimeParams>) -> Self {
        let fft_size = config.processing.fft_size;
        let hop_size = config.hop_size();
        let fifo_cap = MAX_QUANTUM + fft_size;

        Self {
            music_l_port: ptr::null_mut(),
            music_r_port: ptr::null_mut(),
            voice_l_port: ptr::null_mut(),
            voice_r_port: ptr::null_mut(),
            output_l_port: ptr::null_mut(),
            output_r_port: ptr::null_mut(),

            pipeline: DspPipeline::new(config),
            config: config.clone(),
            fft_size,
            hop_size,
            music_gain: db_to_gain(config.output.music_gain_db),
            voice_gain: db_to_gain(config.output.voice_gain_db),
            runtime_params,

            music_window_l: vec![0.0; fft_size],
            music_window_r: vec![0.0; fft_size],
            voice_window_l: vec![0.0; fft_size],
            voice_window_r: vec![0.0; fft_size],

            music_accum_l: vec![0.0; fft_size],
            music_accum_r: vec![0.0; fft_size],
            voice_accum_l: vec![0.0; fft_size],
            voice_accum_r: vec![0.0; fft_size],

            output_l: vec![0.0; fifo_cap],
            output_r: vec![0.0; fifo_cap],
            output_len: 0,

            samples_since_last_hop: 0,

            debug_music_l: Vec::new(),
            debug_music_r: Vec::new(),
            debug_voice_l: Vec::new(),
            debug_voice_r: Vec::new(),
            debug_output_l: Vec::new(),
            debug_output_r: Vec::new(),
            debug_recording: false,
        }
    }

    /// Enable debug recording with pre-allocated buffers.
    pub(crate) fn enable_debug_recording(&mut self) {
        let cap = 48000 * 60; // ~1 minute
        self.debug_music_l = Vec::with_capacity(cap);
        self.debug_music_r = Vec::with_capacity(cap);
        self.debug_voice_l = Vec::with_capacity(cap);
        self.debug_voice_r = Vec::with_capacity(cap);
        self.debug_output_l = Vec::with_capacity(cap);
        self.debug_output_r = Vec::with_capacity(cap);
        self.debug_recording = true;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shift a window left by `chunk.len()` and append the chunk at the end.
#[inline]
pub(crate) fn shift_and_append(window: &mut [f32], chunk: &[f32]) {
    let n = chunk.len();
    let len = window.len();
    window.copy_within(n..len, 0);
    window[len - n..].copy_from_slice(chunk);
}

pub(crate) fn filter_state_name(s: pw_sys::pw_filter_state) -> &'static str {
    #[allow(non_upper_case_globals)]
    match s {
        pw_sys::pw_filter_state_PW_FILTER_STATE_ERROR => "Error",
        pw_sys::pw_filter_state_PW_FILTER_STATE_UNCONNECTED => "Unconnected",
        pw_sys::pw_filter_state_PW_FILTER_STATE_CONNECTING => "Connecting",
        pw_sys::pw_filter_state_PW_FILTER_STATE_PAUSED => "Paused",
        pw_sys::pw_filter_state_PW_FILTER_STATE_STREAMING => "Streaming",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Filter callbacks (run on PipeWire's RT thread)
// ---------------------------------------------------------------------------

/// Process callback — runs every quantum on PipeWire's RT thread.
/// All operations are bounded math / memcpy on pre-allocated buffers.
pub(crate) unsafe extern "C" fn on_process(
    data: *mut c_void,
    position: *mut pipewire::spa::sys::spa_io_position,
) {
    let state = &mut *(data as *mut FilterState);

    if position.is_null() {
        return;
    }
    let n_samples = (*position).clock.duration as usize;
    if n_samples == 0 {
        return;
    }
    let n = n_samples.min(MAX_QUANTUM);

    // Get DSP buffers for all ports (null = port not connected)
    let music_l_ptr =
        pw_sys::pw_filter_get_dsp_buffer(state.music_l_port, n as u32) as *const f32;
    let music_r_ptr =
        pw_sys::pw_filter_get_dsp_buffer(state.music_r_port, n as u32) as *const f32;
    let voice_l_ptr =
        pw_sys::pw_filter_get_dsp_buffer(state.voice_l_port, n as u32) as *const f32;
    let voice_r_ptr =
        pw_sys::pw_filter_get_dsp_buffer(state.voice_r_port, n as u32) as *const f32;
    let out_l_ptr =
        pw_sys::pw_filter_get_dsp_buffer(state.output_l_port, n as u32) as *mut f32;
    let out_r_ptr =
        pw_sys::pw_filter_get_dsp_buffer(state.output_r_port, n as u32) as *mut f32;

    // If output buffers are null, nothing we can do
    if out_l_ptr.is_null() || out_r_ptr.is_null() {
        return;
    }

    let out_l = std::slice::from_raw_parts_mut(out_l_ptr, n);
    let out_r = std::slice::from_raw_parts_mut(out_r_ptr, n);

    // Null input ports → silence (port not connected yet)
    let music_l = if music_l_ptr.is_null() {
        &SILENCE[..n]
    } else {
        std::slice::from_raw_parts(music_l_ptr, n)
    };
    let music_r = if music_r_ptr.is_null() {
        &SILENCE[..n]
    } else {
        std::slice::from_raw_parts(music_r_ptr, n)
    };
    let voice_l = if voice_l_ptr.is_null() {
        &SILENCE[..n]
    } else {
        std::slice::from_raw_parts(voice_l_ptr, n)
    };
    let voice_r = if voice_r_ptr.is_null() {
        &SILENCE[..n]
    } else {
        std::slice::from_raw_parts(voice_r_ptr, n)
    };

    let hop_size = state.hop_size;

    // Feed samples into sliding windows, run pipeline at each hop boundary
    let mut offset = 0;
    while offset < n {
        let remaining_in_hop = hop_size - state.samples_since_last_hop;
        let chunk = remaining_in_hop.min(n - offset);
        let end = offset + chunk;

        shift_and_append(&mut state.music_window_l, &music_l[offset..end]);
        shift_and_append(&mut state.music_window_r, &music_r[offset..end]);
        shift_and_append(&mut state.voice_window_l, &voice_l[offset..end]);
        shift_and_append(&mut state.voice_window_r, &voice_r[offset..end]);

        state.samples_since_last_hop += chunk;
        offset = end;

        if state.samples_since_last_hop == hop_size {
            state.samples_since_last_hop = 0;

            // Run the full DSP pipeline — synthesize() ADDS to accumulators,
            // overlapping with the tail of the previous frame.
            // Read runtime params (lock-free atomics)
            let rt_compression_mix = state.runtime_params.load_compression_mix();
            let rt_voice_gain = state.runtime_params.load_voice_gain();
            let rt_masking_depth = state.runtime_params.load_masking_depth();
            let rt_wideband = state.runtime_params.load_wideband();

            // Override masking config with runtime values.
            // The masking intensity slider (0-1) drives both depth and
            // max_reduction_db together so the full range is useful:
            //   0%  → depth=0, max_reduction= 0 dB (no ducking)
            //   50% → depth=0.5, max_reduction=-12 dB
            //   90% → depth=0.9, max_reduction=-21.6 dB
            //  100% → depth=1.0, max_reduction=-24 dB
            let mut masking = state.config.masking.clone();
            masking.depth = rt_masking_depth;
            masking.max_reduction_db = -24.0 * rt_masking_depth;
            masking.focus_strength = rt_wideband;

            state.pipeline.process_frame(
                &state.music_window_l,
                &state.music_window_r,
                &state.voice_window_l,
                &state.voice_window_r,
                &masking,
                &mut state.music_accum_l,
                &mut state.music_accum_r,
                &mut state.voice_accum_l,
                &mut state.voice_accum_r,
                0,
                rt_compression_mix,
            );

            // Mix first hop_size from accumulators → append to output FIFO
            let fifo_pos = state.output_len;
            let mg = state.music_gain;
            let vg = rt_voice_gain;
            for i in 0..hop_size {
                state.output_l[fifo_pos + i] =
                    state.music_accum_l[i] * mg + state.voice_accum_l[i] * vg;
                state.output_r[fifo_pos + i] =
                    state.music_accum_r[i] * mg + state.voice_accum_r[i] * vg;
            }
            state.output_len += hop_size;

            // Shift accumulators left by hop_size (discard consumed samples),
            // zero the tail where the next frame will overlap-add.
            let fft = state.fft_size;
            state.music_accum_l.copy_within(hop_size..fft, 0);
            state.music_accum_l[fft - hop_size..].fill(0.0);
            state.music_accum_r.copy_within(hop_size..fft, 0);
            state.music_accum_r[fft - hop_size..].fill(0.0);
            state.voice_accum_l.copy_within(hop_size..fft, 0);
            state.voice_accum_l[fft - hop_size..].fill(0.0);
            state.voice_accum_r.copy_within(hop_size..fft, 0);
            state.voice_accum_r[fft - hop_size..].fill(0.0);
        }
    }

    // Drain FIFO into PipeWire output buffers
    let to_drain = state.output_len.min(n);
    out_l[..to_drain].copy_from_slice(&state.output_l[..to_drain]);
    out_r[..to_drain].copy_from_slice(&state.output_r[..to_drain]);

    // Shift remaining FIFO data left
    if to_drain < state.output_len {
        state.output_l.copy_within(to_drain..state.output_len, 0);
        state.output_r.copy_within(to_drain..state.output_len, 0);
    }
    state.output_len -= to_drain;

    // Pad remainder with silence (startup latency)
    out_l[to_drain..].fill(0.0);
    out_r[to_drain..].fill(0.0);

    // Debug recording: capture inputs and outputs from this cycle
    if state.debug_recording {
        state.debug_music_l.extend_from_slice(music_l);
        state.debug_music_r.extend_from_slice(music_r);
        state.debug_voice_l.extend_from_slice(voice_l);
        state.debug_voice_r.extend_from_slice(voice_r);
        state.debug_output_l.extend_from_slice(out_l);
        state.debug_output_r.extend_from_slice(out_r);
    }
}

/// State-changed callback — logs filter state transitions.
pub(crate) unsafe extern "C" fn on_state_changed(
    _data: *mut c_void,
    old: pw_sys::pw_filter_state,
    new: pw_sys::pw_filter_state,
    error: *const c_char,
) {
    let err_msg = if error.is_null() {
        String::new()
    } else {
        format!(": {}", std::ffi::CStr::from_ptr(error).to_string_lossy())
    };
    eprintln!(
        "  Filter state: {} -> {}{}",
        filter_state_name(old),
        filter_state_name(new),
        err_msg
    );
}

// ---------------------------------------------------------------------------
// Port creation helper
// ---------------------------------------------------------------------------

/// Add a DSP audio port (mono f32) to the filter.
pub(crate) unsafe fn add_port(
    filter: *mut pw_sys::pw_filter,
    direction: u32,
    name: &str,
) -> *mut c_void {
    let port_props = properties! {
        "format.dsp" => "32 bit float mono audio",
        "port.name" => name
    };

    pw_sys::pw_filter_add_port(
        filter,
        direction,
        pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
        0, // port_data_size — we store handles externally
        port_props.into_raw(),
        ptr::null_mut(),
        0,
    )
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the PipeWire filter node. Blocks until Ctrl+C.
pub fn run_pipewire(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let fft_size = config.processing.fft_size;
    let hop_size = config.hop_size();
    let sample_rate = config.processing.sample_rate;

    eprintln!("SpectralBlend PipeWire filter node");
    eprintln!("  FFT size: {fft_size}, hop: {hop_size}, sample rate: {sample_rate}");
    eprintln!("  Music gain: {:.1} dB, Voice gain: {:.1} dB",
        config.output.music_gain_db, config.output.voice_gain_db);

    // Initialize PipeWire
    pipewire::init();

    // Create main loop (safe API)
    let mainloop = MainLoopBox::new(None)?;

    // Register SIGINT handler to quit the loop cleanly
    let mainloop_weak = mainloop.as_ref() as *const pipewire::main_loop::MainLoop;
    let _sigint = mainloop.loop_().add_signal_local(
        pipewire::loop_::Signal::SIGINT,
        move || unsafe { (*mainloop_weak).quit() },
    );
    let _sigterm = mainloop.loop_().add_signal_local(
        pipewire::loop_::Signal::SIGTERM,
        move || unsafe { (*mainloop_weak).quit() },
    );

    // Allocate processing state
    let params = Arc::new(RuntimeParams::new(config));
    let mut state = Box::new(FilterState::new(config, params));

    // Build the pw_filter_events struct
    let mut events: pw_sys::pw_filter_events = unsafe { std::mem::zeroed() };
    events.version = pw_sys::PW_VERSION_FILTER_EVENTS;
    events.process = Some(on_process);
    events.state_changed = Some(on_state_changed);

    // Create node properties
    let node_props = properties! {
        "media.type" => "Audio",
        "media.category" => "Filter",
        "media.role" => "DSP",
        "node.name" => "SpectralBlend",
        "node.description" => "Spectral voice masking filter"
    };

    let filter_name = CString::new("SpectralBlend").unwrap();

    // Create the filter node
    let state_ptr = &mut *state as *mut FilterState as *mut c_void;
    let filter = unsafe {
        pw_sys::pw_filter_new_simple(
            mainloop.loop_().as_raw_ptr(),
            filter_name.as_ptr(),
            node_props.into_raw(),
            &events as *const pw_sys::pw_filter_events,
            state_ptr,
        )
    };
    if filter.is_null() {
        return Err("Failed to create PipeWire filter".into());
    }

    // Add ports: 4 input (music L/R, voice L/R) + 2 output (mix L/R)
    unsafe {
        state.music_l_port = add_port(
            filter,
            pipewire::spa::sys::SPA_DIRECTION_INPUT,
            "music_left",
        );
        state.music_r_port = add_port(
            filter,
            pipewire::spa::sys::SPA_DIRECTION_INPUT,
            "music_right",
        );
        state.voice_l_port = add_port(
            filter,
            pipewire::spa::sys::SPA_DIRECTION_INPUT,
            "voice_left",
        );
        state.voice_r_port = add_port(
            filter,
            pipewire::spa::sys::SPA_DIRECTION_INPUT,
            "voice_right",
        );
        state.output_l_port = add_port(
            filter,
            pipewire::spa::sys::SPA_DIRECTION_OUTPUT,
            "output_left",
        );
        state.output_r_port = add_port(
            filter,
            pipewire::spa::sys::SPA_DIRECTION_OUTPUT,
            "output_right",
        );
    }

    // Verify all ports were created
    if state.music_l_port.is_null()
        || state.music_r_port.is_null()
        || state.voice_l_port.is_null()
        || state.voice_r_port.is_null()
        || state.output_l_port.is_null()
        || state.output_r_port.is_null()
    {
        unsafe { pw_sys::pw_filter_destroy(filter) };
        return Err("Failed to create one or more filter ports".into());
    }

    // Connect the filter with RT processing
    let ret = unsafe {
        pw_sys::pw_filter_connect(
            filter,
            pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
            ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        unsafe { pw_sys::pw_filter_destroy(filter) };
        return Err(format!("Failed to connect filter: error {ret}").into());
    }

    eprintln!("  Ports: music_left, music_right, voice_left, voice_right -> output_left, output_right");
    eprintln!("  Use `pw-link -l` to see ports, `pw-link` to connect them.");
    eprintln!("  Processing... (Ctrl+C to stop)");

    // Enter the main loop — blocks until quit
    mainloop.run();

    // Clean up
    eprintln!("\nStopping...");
    unsafe { pw_sys::pw_filter_destroy(filter) };

    // state (Box<FilterState>) drops here automatically
    drop(state);

    eprintln!("Done.");
    Ok(())
}
