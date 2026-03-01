# Project: SpectralBlend — Real-Time Spectral Voice Masking for PipeWire

## What You Are Building

You are building a real-time audio processing application for Linux that runs as a PipeWire filter node. Its purpose is to allow voice chat audio (e.g., from Discord) to blend seamlessly into a music stream without the user ever perceiving a volume change in their music. It does this through **spectral masking**: analyzing the voice signal's frequency content in real-time and surgically reducing only those exact frequency bins in the music where voice energy is present, then mixing the clean voice into the resulting gap.

This is NOT ducking (global volume reduction). This is NOT ANC/phase cancellation. This is **frequency-domain sidechain spectral subtraction** — a DSP technique where the voice signal's spectral envelope is used as a real-time gain mask applied to the music signal.

### Why This Exists

The user has a neurodivergent auditory processing profile where:
- Music occupies nearly all cognitive bandwidth — it's not background, it's a primary focus
- Traditional ducking (lowering music volume when voice is detected) is immediately noticeable and causes more distraction than the problem it solves
- The goal is to make voices feel like they were **always part of the music mix**, not competing with it
- Between speech, the music must be 100% untouched — zero artifacts, zero volume changes
- The user has specific hearing characteristics requiring preserved sub-bass energy (below 80Hz must never be touched)

### The Perceptual Goal

When someone speaks on Discord while music is playing, the listener should perceive the voice as if a mix engineer had carefully EQ'd a space for it in the mix. When nobody is speaking, the music should be bit-perfect / indistinguishable from the original. The transition between "someone speaking" and "nobody speaking" must be completely smooth with no audible pumping, clicking, or gating artifacts.

---

## Technical Architecture Overview

```
┌─────────────────┐     ┌─────────────────────────────────────────┐     ┌──────────┐
│  Music Source    │────▶│          SpectralBlend Node              │────▶│ Headphone│
│  (all apps)     │     │                                         │     │ Output   │
└─────────────────┘     │  ┌─────────┐  ┌──────────┐  ┌───────┐  │     └──────────┘
                        │  │ FFT     │  │ Spectral │  │ IFFT  │  │
┌─────────────────┐     │  │ Analysis│─▶│ Masking  │─▶│ + Mix │  │
│  Voice Source   │────▶│  │ (both)  │  │ Engine   │  │       │  │
│  (Discord etc.) │     │  └─────────┘  └──────────┘  └───────┘  │
└─────────────────┘     └─────────────────────────────────────────┘
```

The application is a **PipeWire filter-chain node** (or standalone PipeWire client) with:
- **Input A (Music Bus):** Captures the mixed output of all non-voice applications
- **Input B (Voice Bus):** Captures only the voice chat application output (Discord, TeamSpeak, etc.)
- **Output:** Single stereo stream going to the user's headphone/DAC output

---

## Core DSP Pipeline — Step by Step

### 1. Windowed FFT Analysis (both streams)

For each audio block (both music and voice), apply a **Hann window** and perform a real-valued FFT.

- **FFT size:** 2048 samples (at 48kHz this gives ~42ms window, ~21ms hop with 50% overlap)
- **Hop size:** 1024 samples (50% overlap for smooth reconstruction)
- **Window function:** Hann window — critical for overlap-add reconstruction without artifacts
- Both music and voice get the same FFT treatment independently

The output is complex-valued frequency bins. Extract the **magnitude spectrum** from both:
```
music_mag[k] = |Music_FFT[k]|
voice_mag[k] = |Voice_FFT[k]|
```

### 2. Voice Activity & Spectral Gate

Before applying any masking, determine if meaningful voice energy is present. This prevents the music from being affected by Discord's noise floor, digital silence, or quiet room tone.

- Compute the RMS energy of the voice magnitude spectrum
- Apply a **soft gate** with hysteresis: voice is "active" when energy exceeds an upper threshold, and "inactive" when it drops below a lower threshold
- Use a **smooth envelope follower** (attack ~5ms, release ~80-150ms) on the gate signal to avoid hard transitions
- When the gate is fully closed, the music passes through completely untouched (multiply mask by 1.0 everywhere)

Research: Look into voice activity detection (VAD) algorithms — even a simple spectral flux or zero-crossing-rate based VAD would improve robustness over raw RMS. WebRTC's VAD algorithm is well-documented and could serve as inspiration.

### 3. Spectral Mask Construction

This is the heart of the algorithm.

For each frequency bin `k`:
```
# Raw masking ratio: how much voice energy exists relative to music at this bin
ratio[k] = voice_mag[k] / (music_mag[k] + epsilon)

# Scale by a user-controllable depth parameter (0.0 = no masking, 1.0 = full masking)
mask[k] = 1.0 - (depth * clamp(ratio[k] * sensitivity, 0.0, max_reduction))

# Where:
#   depth:         overall effect strength (default 0.6 — not full subtraction)
#   sensitivity:   how aggressively small voice signals trigger masking (default 1.5)
#   max_reduction: maximum dB of reduction per bin (default 0.7 = ~3dB, never silence a bin)
#   epsilon:       small value to avoid division by zero (1e-10)
```

**Critical constraints on the mask:**
- **Floor value:** No bin should ever be reduced below -6dB to -10dB (user-configurable). This prevents "hollow" artifacts where a frequency band is completely removed.
- **Sub-bass protection:** Bins below 80Hz must have their mask pinned to 1.0 (no reduction). The user has specific hearing needs — sub-bass must never be touched.
- **Frequency range focus:** Primary masking should focus on the 300Hz–6kHz range where voice fundamentals and intelligibility formants live. Gradually taper masking strength outside this range using a bandpass-shaped weighting curve.
- **Spectral smoothing:** Apply a small moving average (3-5 bins wide) across the mask to avoid isolated sharp notches that can cause ringing artifacts.

### 4. Temporal Smoothing of the Mask

The mask must be smoothed over time to prevent frame-to-frame jitter:

```
smoothed_mask[k] = alpha * smoothed_mask_prev[k] + (1 - alpha) * mask[k]

# Where alpha depends on direction:
#   If mask is decreasing (more masking): alpha_attack ≈ 0.3 (fast, ~5ms)
#   If mask is increasing (releasing):    alpha_release ≈ 0.85 (slow, ~80ms)
```

This asymmetric smoothing means masking engages quickly when speech starts but releases gently, avoiding the "pumping" effect that makes ducking so noticeable.

### 5. Apply Mask and Reconstruct via Overlap-Add

```
masked_music_fft[k] = Music_FFT[k] * smoothed_mask[k]   # Complex multiply by real mask
```

Then IFFT the masked music, apply the synthesis window, and overlap-add to reconstruct the time-domain signal.

**Output = masked_music + clean_voice** (mixed at configurable voice gain level)

Research: Look into the **Weighted Overlap-Add (WOLA)** method as a potentially better reconstruction approach. Also research whether 75% overlap (hop = FFT/4) would give smoother results at the cost of more CPU — for this use case it might be worth it.

---

## PipeWire Integration

### Approach A: Native PipeWire Filter-Chain (Recommended to Research First)

PipeWire has a built-in `filter-chain` module that can load LADSPA/LV2 plugins or run Lua-based DSP. Investigate whether the filter-chain can be configured with **two separate input streams** (music + voice) routed to a single processing node. This would be the cleanest integration.

Relevant PipeWire documentation and concepts to research:
- `pw-filter` API for writing custom filter nodes in C
- `libpipewire` client API for creating processing nodes
- PipeWire's SPA (Simple Plugin API) for audio buffer handling
- How to create a node with 4 input ports (2x stereo) and 2 output ports (1x stereo)
- PipeWire session manager (WirePlumber) rules for automatic stream routing

### Approach B: Standalone PipeWire Client Application

Write a standalone application using `libpipewire` that:
1. Creates a PipeWire client node
2. Registers 4 input ports: `music_left`, `music_right`, `voice_left`, `voice_right`
3. Registers 2 output ports: `output_left`, `output_right`
4. In the `process` callback, runs the spectral masking DSP pipeline
5. Uses WirePlumber or `pw-link` to route audio streams to the correct ports

The user would then configure their system so that:
- Discord's output → SpectralBlend voice input ports
- All other audio → SpectralBlend music input ports (via a PipeWire loopback or virtual sink)
- SpectralBlend output → actual hardware output

### Approach C: LV2 Plugin Loaded by PipeWire Filter-Chain

Build the DSP engine as an LV2 plugin with sidechain input, then load it via PipeWire's filter-chain configuration. This has the advantage of also being usable in other hosts (Carla, Ardour, etc).

Research which approach gives the best latency characteristics and easiest routing setup. Approach B is probably most flexible, but look into all three.

---

## Language and Dependencies

### Recommended: Rust

- **`pipewire-rs`** — Rust bindings for libpipewire (crate: `pipewire`). Research the current state of these bindings and whether they support custom filter nodes with multiple port groups.
- **`rustfft`** — High-performance FFT library in pure Rust. Well-maintained, no unsafe code, very fast.
- **`dasp`** — Digital audio signal processing primitives for Rust (sample format conversion, ring buffers, etc.)
- **FFTW bindings (`fftw` crate)** — Alternative to rustfft if maximum performance is needed; wraps the FFTW3 C library.

### Alternative: C

If Rust PipeWire bindings prove insufficient:
- **`libpipewire`** — Native C API (most complete, best documented)
- **`libfftw3`** or **`kissfft`** — FFT libraries. KissFFT is simpler and header-only; FFTW3 is faster.
- **`libsndfile`** — Only needed if implementing file-based testing/debugging

### Build System
- Rust: Cargo with `pkg-config` for finding PipeWire headers/libs
- C: Meson or CMake, linking against `libpipewire-0.3`

### Runtime Dependencies
- PipeWire >= 0.3.x (running as the audio server)
- WirePlumber (for automatic stream routing rules)

---

## Configuration and Tunables

Provide a configuration file (TOML or JSON) and/or command-line arguments for:

```toml
[processing]
fft_size = 2048              # FFT window size in samples
hop_ratio = 0.5              # Overlap ratio (0.5 = 50%, 0.25 = 75%)
sample_rate = 48000          # Must match PipeWire graph sample rate

[masking]
depth = 0.6                  # Overall masking strength (0.0–1.0)
sensitivity = 1.5            # Voice detection sensitivity multiplier
max_reduction_db = -6.0      # Maximum reduction per bin in dB (never go below this)
sub_bass_protect_hz = 80.0   # Below this frequency, mask is always 1.0
focus_low_hz = 300.0         # Lower edge of primary masking band
focus_high_hz = 6000.0       # Upper edge of primary masking band
spectral_smooth_bins = 3     # Number of bins for mask smoothing

[envelope]
attack_ms = 5.0              # How fast masking engages
release_ms = 100.0           # How slowly masking releases
gate_threshold_on = -40.0    # dB threshold to activate voice gate
gate_threshold_off = -50.0   # dB threshold to deactivate voice gate

[output]
voice_gain_db = 0.0          # Gain applied to voice before mixing
music_gain_db = 0.0          # Gain applied to music output
```

---

## Routing Setup Helper

The application should include a helper script or subcommand (`spectralblend setup` or similar) that:

1. Detects running PipeWire and WirePlumber
2. Creates a virtual sink called "SpectralBlend-Music" that captures all non-voice audio
3. Creates a virtual sink called "SpectralBlend-Voice" that the user assigns Discord to
4. Links these sinks to SpectralBlend's input ports
5. Links SpectralBlend's output to the user's default audio output
6. Optionally generates a WirePlumber Lua rule that automatically routes `discord` (or other configurable app names by PipeWire `application.name` property) to the voice sink

---

## Testing and Validation

Implement a **file-based test mode** where:
- The user provides a music WAV file and a voice WAV file
- SpectralBlend processes them offline and writes the output to a new WAV file
- This allows tuning parameters by ear without needing live PipeWire routing

Also implement a **debug mode** that dumps per-frame data:
- Voice gate state (open/closed/transitioning)
- Average mask value across the voice band
- Peak reduction in dB
- CPU usage per process cycle

---

## Stretch Goals and Research Directions

When implementing, research these topics to potentially improve the design:

1. **Perceptual frequency scaling:** Instead of linear FFT bins, consider implementing masking on a **Bark scale** or **ERB (Equivalent Rectangular Bandwidth) scale** that matches human auditory perception. This would make the masking more perceptually uniform. Research how to map FFT bins to Bark/ERB bands efficiently.

2. **Phase-aware reconstruction:** The basic algorithm only modifies magnitudes and preserves original phases. Research whether **phase interpolation** or **phase-gradient methods** could reduce artifacts during heavy masking.

3. **Multi-channel voice separation:** If multiple people are speaking simultaneously on Discord, their combined energy causes broader masking. Research whether a pre-processing step could estimate the number of simultaneous speakers and adjust masking depth accordingly to avoid over-masking.

4. **Adaptive masking depth:** Instead of a fixed depth parameter, research whether the masking depth could automatically adjust based on the music's spectral density in the voice band. Dense mix = less masking (voice would be inaudible anyway), sparse mix = more masking (less music to displace, so go deeper). This would be self-optimizing.

5. **Latency optimization:** Research SIMD-accelerated FFT (AVX2/AVX-512 on the user's Intel Core Ultra 7 265K) and whether a smaller FFT size (1024) with higher overlap could achieve lower latency while maintaining quality. Target total added latency under 20ms.

6. **Psychoacoustic masking models:** Research the **MPEG psychoacoustic model** (used in MP3/AAC encoding) and the concept of simultaneous masking thresholds. The voice might already be perceptually masked by loud music at certain frequencies — in those cases, the algorithm should avoid cutting music there since the voice wouldn't be heard anyway. This would minimize unnecessary modifications to the music.

7. **Look into existing implementations:** Search for "spectral subtraction sidechain," "frequency domain ducking," "spectral masking audio plugin," and "sidechain FFT processing" to see if anyone has already implemented similar concepts as LADSPA/LV2/VST plugins that could be studied or adapted.

---

## Project Structure Suggestion

```
spectralblend/
├── Cargo.toml
├── README.md
├── config.toml                  # Default configuration
├── src/
│   ├── main.rs                  # Entry point, CLI argument parsing
│   ├── config.rs                # Configuration loading and validation
│   ├── pipewire_node.rs         # PipeWire client node setup and port management
│   ├── dsp/
│   │   ├── mod.rs
│   │   ├── fft.rs               # FFT/IFFT, windowing, overlap-add
│   │   ├── spectral_mask.rs     # Mask construction, smoothing, constraints
│   │   ├── voice_gate.rs        # Voice activity detection and gating
│   │   └── mixer.rs             # Final mixing of masked music + voice
│   ├── routing/
│   │   ├── mod.rs
│   │   └── setup.rs             # Virtual sink creation and WirePlumber rules
│   └── debug.rs                 # Debug logging and stats output
├── scripts/
│   └── wireplumber-rule.lua     # Example WirePlumber routing rule
└── tests/
    ├── offline_test.rs          # File-based processing tests
    └── test_audio/              # Sample WAV files for testing
```

---

## Summary for Claude Code

You are building **SpectralBlend**, a real-time PipeWire audio processor written in Rust that makes voice chat blend invisibly into music using spectral masking. The voice signal's frequency content is analyzed via FFT and used to carve precisely shaped, temporary, minimal gaps in the music's spectrum where voice energy exists. When nobody is speaking, the music is completely unaffected. This is categorically different from volume ducking — it is frequency-domain surgical intervention that should be perceptually invisible.

Start by:
1. Searching for documentation on `pipewire-rs` crate capabilities for multi-port filter nodes
2. Searching for existing spectral masking / spectral subtraction implementations in audio plugins
3. Implementing the core DSP pipeline with offline WAV file testing first
4. Then integrating with PipeWire as a live filter node
5. Adding the routing setup helper last

Prioritize audio quality and artifact-free output above all else. Latency under 20ms is the target. The user is an experienced Linux system administrator and audio producer who will be critically listening — artifacts will not be acceptable.
