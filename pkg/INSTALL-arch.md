# Installing SpectralBlend on Arch Linux

## Prerequisites

Install build dependencies:

```sh
sudo pacman -S --needed rust clang pipewire libxcb wayland libxkbcommon mesa
```

## Build & install

Clone the repo and build:

```sh
git clone https://github.com/ilovemicrowaves/IntelliVoice.git
cd IntelliVoice
cargo build --release --features gui
```

Install the binary, desktop entry, and icon:

```sh
sudo install -Dm755 target/release/spectralblend /usr/bin/spectralblend
sudo install -Dm644 pkg/spectralblend.desktop /usr/share/applications/spectralblend.desktop
sudo install -Dm644 pkg/spectralblend.svg /usr/share/icons/hicolor/scalable/apps/spectralblend.svg
```

## Run

Launch from your app menu (search "SpectralBlend") or from a terminal:

```sh
spectralblend
```

This opens the GUI by default. Subcommands are also available:

```sh
spectralblend offline --help
spectralblend pipewire --help
```

## Uninstall

```sh
sudo rm /usr/bin/spectralblend
sudo rm /usr/share/applications/spectralblend.desktop
sudo rm /usr/share/icons/hicolor/scalable/apps/spectralblend.svg
```
