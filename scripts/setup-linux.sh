#!/usr/bin/env bash
# One-time Linux setup for building/running Joi (PLAN §3, §9).
#
# Installs the WebKitGTK build libs and — critically — the GStreamer plugins WebKitGTK needs for
# getUserMedia/getDisplayMedia (the part most setups miss), plus Rust components and tauri-cli.
# Detects apt vs pacman. Requires sudo for the system packages.
set -euo pipefail

# Debian/Ubuntu package names (PLAN §3).
APT_PKGS=(
  libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev
  libssl-dev build-essential pkg-config curl wget file
  libgstreamer1.0-dev gstreamer1.0-plugins-base gstreamer1.0-plugins-good
  gstreamer1.0-plugins-bad gstreamer1.0-libav gstreamer1.0-pipewire
  xdg-desktop-portal xdg-desktop-portal-gtk
)

# Arch/CachyOS equivalents.
PACMAN_PKGS=(
  webkit2gtk-4.1 gtk3 librsvg libayatana-appindicator openssl base-devel pkgconf curl wget file
  gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav gst-plugin-pipewire
  xdg-desktop-portal xdg-desktop-portal-gtk
)

if command -v apt-get >/dev/null 2>&1; then
  sudo apt-get update
  sudo apt-get install -y "${APT_PKGS[@]}"
elif command -v pacman >/dev/null 2>&1; then
  sudo pacman -S --needed "${PACMAN_PKGS[@]}"
else
  echo "Unsupported distro: install the WebKitGTK + GStreamer deps manually (see PLAN §3)." >&2
  exit 1
fi

# Rust toolchain components (rust-toolchain.toml pins the channel).
rustup component add rustfmt clippy || true
cargo install tauri-cli --version '^2' || true

echo
echo "Setup done. Install JS deps with:  bun install   (or: pnpm install)"
echo
echo "Display-server note (PLAN §3, B1): WebKitGTK media capture is fragile on Wayland."
echo "If mic/screen capture misbehaves, launch with the X11 fallback:"
echo "  GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 cargo tauri dev"
