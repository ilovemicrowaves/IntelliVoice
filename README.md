# SpectralBlend

Real-time spectral voice masking for Linux. SpectralBlend ducks music frequencies where voice is present so you hear both clearly — like sidechain compression, but surgical and frequency-aware.

Built for Discord, podcasts, streams, or any scenario where you're talking over music.

## How it works

SpectralBlend runs as a PipeWire filter node. Route your music and voice inputs through it, and the output is a clean mix where the music automatically ducks out of the way of your voice — only in the frequency bands where they'd clash.

The DSP pipeline: voice compression → FFT → adaptive voice gate → spectral masking → IFFT → overlap-add → mix.

## Features

- **GUI mode** — sliders for compression, voice gain, masking depth, and wideband/focused blend
- **PipeWire filter node** — integrates natively with your Linux audio graph, route any app through it with `pw-link`
- **Adaptive voice gate** — tracks ambient noise floor automatically, no manual threshold tuning
- **Spectral masking** — per-frequency-bin ducking, not broadband compression
- **Stereo I/O** — independent L/R processing
- **Offline mode** — process WAV files for testing and tuning
- **CPAL real-time mode** — works with ALSA/JACK devices directly
- **Configurable** — optional TOML config file for all DSP parameters

## Modes

| Mode | Feature flag | Description |
|------|-------------|-------------|
| `spectralblend` | `gui` | GUI with PipeWire routing (default when no subcommand) |
| `spectralblend pipewire` | `pipewire` | Headless PipeWire filter node |
| `spectralblend realtime` | *(none)* | CPAL-based real-time processing |
| `spectralblend offline` | *(none)* | Process WAV files |
| `spectralblend devices` | *(none)* | List audio devices |

## Building from source

### Requirements (all distros)

- Rust toolchain (1.70+)
- `clang` (needed by bindgen for PipeWire FFI)
- PipeWire development headers
- Wayland/X11 libraries (for the GUI)

### Ubuntu / Debian

Install dependencies:

```sh
sudo apt install build-essential clang libpipewire-0.3-dev libxcb1-dev libwayland-dev libxkbcommon-dev libgl1-mesa-dev
```

Build and install:

```sh
git clone https://github.com/ilovemicrowaves/IntelliVoice.git
cd IntelliVoice
cargo build --release --features gui
sudo install -Dm755 target/release/spectralblend /usr/bin/spectralblend
sudo install -Dm644 pkg/spectralblend.desktop /usr/share/applications/spectralblend.desktop
sudo install -Dm644 pkg/spectralblend.svg /usr/share/icons/hicolor/scalable/apps/spectralblend.svg
```

Or build a .deb package:

```sh
cargo install cargo-deb
cargo deb
sudo dpkg -i target/debian/spectralblend_0.1.0-1_amd64.deb
```

### Arch Linux

Install dependencies:

```sh
sudo pacman -S --needed rust clang pipewire libxcb wayland libxkbcommon mesa
```

Build and install:

```sh
git clone https://github.com/ilovemicrowaves/IntelliVoice.git
cd IntelliVoice
cargo build --release --features gui
sudo install -Dm755 target/release/spectralblend /usr/bin/spectralblend
sudo install -Dm644 pkg/spectralblend.desktop /usr/share/applications/spectralblend.desktop
sudo install -Dm644 pkg/spectralblend.svg /usr/share/icons/hicolor/scalable/apps/spectralblend.svg
```

A `PKGBUILD` is also available in `pkg/` for AUR-style packaging.

### Fedora

Install dependencies:

```sh
sudo dnf install gcc clang pipewire-devel libxcb-devel wayland-devel libxkbcommon-devel mesa-libGL-devel
```

Build and install:

```sh
git clone https://github.com/ilovemicrowaves/IntelliVoice.git
cd IntelliVoice
cargo build --release --features gui
sudo install -Dm755 target/release/spectralblend /usr/bin/spectralblend
sudo install -Dm644 pkg/spectralblend.desktop /usr/share/applications/spectralblend.desktop
sudo install -Dm644 pkg/spectralblend.svg /usr/share/icons/hicolor/scalable/apps/spectralblend.svg
```

### Without GUI

To build without the GUI (headless PipeWire filter only):

```sh
cargo build --release --features pipewire
```

To build with only offline/CPAL modes (no PipeWire dependency):

```sh
cargo build --release
```

## Configuration

SpectralBlend works out of the box with sensible defaults. To customize, pass a TOML config file:

```sh
spectralblend --config my-config.toml
```

See the source for all available parameters (`src/config.rs`).

## Uninstall

```sh
sudo rm /usr/bin/spectralblend
sudo rm /usr/share/applications/spectralblend.desktop
sudo rm /usr/share/icons/hicolor/scalable/apps/spectralblend.svg
```

Or if installed via .deb: `sudo apt remove spectralblend`

## License

MIT
