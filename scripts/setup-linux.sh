#!/usr/bin/env bash
# One-time Linux setup for building/running Joi (PLAN-NATIVE-MEDIA §7).
#
# Installs the WebKitGTK build libs (for the UI webview) plus the ALSA dev headers cpal needs to
# build native audio. Media is native (cpal capture/playback, xcap screen) — there is NO webview
# media path anymore, so GStreamer is NOT required. PipeWire's ALSA compatibility layer (pipewire-alsa)
# is what cpal talks to on modern desktops; install it if your audio runs on PipeWire.
# Detects apt vs pacman. Requires sudo for the system packages.
set -euo pipefail

# Debian/Ubuntu package names.
APT_PKGS=(
  libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev
  libssl-dev build-essential pkg-config curl wget file
  libasound2-dev          # ALSA dev headers — cpal links these at build time
  pipewire-alsa           # PipeWire's ALSA compat (skip if you run plain ALSA/PulseAudio)
)

# Arch/CachyOS equivalents.
PACMAN_PKGS=(
  webkit2gtk-4.1 gtk3 librsvg libayatana-appindicator openssl base-devel pkgconf curl wget file
  alsa-lib                # ALSA dev headers — cpal links these at build time
  pipewire-alsa           # PipeWire's ALSA compat (skip if you run plain ALSA/PulseAudio)
)

if command -v apt-get >/dev/null 2>&1; then
  sudo apt-get update
  sudo apt-get install -y "${APT_PKGS[@]}"
elif command -v pacman >/dev/null 2>&1; then
  sudo pacman -S --needed "${PACMAN_PKGS[@]}"
else
  echo "Unsupported distro: install the WebKitGTK libs + ALSA dev headers (alsa-lib/libasound2-dev)" >&2
  echo "manually, plus pipewire-alsa if you run PipeWire. See PLAN-NATIVE-MEDIA §7." >&2
  exit 1
fi

# Rust toolchain components (rust-toolchain.toml pins the channel).
rustup component add rustfmt clippy || true
cargo install tauri-cli --version '^2' || true

echo
echo "Setup done. Install JS deps with:  bun install   (or: pnpm install)"
echo
echo "Audio note: capture/playback are native via cpal → ALSA (PipeWire's ALSA compat works)."
echo "Set the input/output device in config if the default device isn't right."
