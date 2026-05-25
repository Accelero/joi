#!/usr/bin/env bash
# Mirror the CI quality gate locally. Run before every commit.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "── cargo fmt ──"
cargo fmt --all --check

echo "── cargo clippy (-D warnings) ──"
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets

echo "── cargo test ──"
cargo test --workspace

# Layering is honest only if the dependency graph proves it. These assertions are
# the architectural gate: they fail the build if a crate grows a dependency it must never have.
echo "── dependency assertions (layering) ──"

# joi-core is the pure domain: no device I/O, no provider SDK, no host/UI, no HTTP.
core_tree="$(cargo tree -p joi-core -e no-dev 2>/dev/null)"
if echo "$core_tree" | grep -qiE 'cpal|xcap|sonora|adk-realtime|reqwest|ratatui|crossterm|joi-tools'; then
  echo "!! joi-core must not depend on devices/providers/HTTP/UI (cpal/xcap/sonora/adk-realtime/reqwest/ratatui)" >&2
  echo "$core_tree" | grep -iE 'cpal|xcap|sonora|adk-realtime|reqwest|ratatui|crossterm|joi-tools' >&2
  exit 1
fi

# Terminal UI deps live only in joi-tui.
for crate in joi-core joi-tools joi-providers joi-media joi-app; do
  if cargo tree -p "$crate" -e no-dev 2>/dev/null | grep -qiE 'ratatui|crossterm'; then
    echo "!! $crate must not depend on ratatui/crossterm — terminal UI belongs to joi-tui only" >&2
    exit 1
  fi
done

# No host/webview/binding-generator anywhere in the workspace.
for crate in joi-core joi-tools joi-providers joi-media joi-app joi-tui; do
  if cargo tree -p "$crate" -e no-dev 2>/dev/null | grep -qiE 'tauri|webkit|ts-rs'; then
    echo "!! $crate must not depend on tauri/webkit/ts-rs — this is a native, TUI-first rewrite" >&2
    exit 1
  fi
done

echo "✓ all checks passed"
