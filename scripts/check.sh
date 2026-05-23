#!/usr/bin/env bash
# Mirror the CI quality gate locally (PLAN §8, §9). Run before every commit.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "── cargo fmt ──"
cargo fmt --all --check

echo "── cargo clippy (-D warnings) ──"
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets

echo "── cargo test ──"
cargo test --workspace

# `cargo test` regenerates the ts-rs IPC bindings (joi-core `#[ts(export)]` → src/bindings). If the
# committed copies differ, the Rust types and the frontend's TS contract have drifted — fail loudly.
echo "── generated TS bindings are up to date (src/bindings) ──"
if [ -n "$(git status --porcelain src/bindings)" ]; then
  echo "!! src/bindings is out of date — run 'cargo test -p joi-core' and commit the result" >&2
  git --no-pager diff -- src/bindings >&2
  git --no-pager status --porcelain src/bindings >&2
  exit 1
fi

echo "── engine stays host-agnostic (joi-app/joi-cli/joi-server must not depend on Tauri) ──"
for crate in joi-app joi-cli joi-server; do
  if cargo tree -p "$crate" -e no-dev 2>/dev/null | grep -qiE 'tauri|webkit'; then
    echo "!! $crate must not depend on Tauri/WebKit — the JOI engine must be host-agnostic" >&2
    exit 1
  fi
done

# Frontend gate. Requires bun (or swap for pnpm). Skipped if bun is unavailable.
if command -v bun >/dev/null 2>&1; then
  echo "── frontend typecheck / test / build ──"
  bun run typecheck
  bun run test
  bun run build
else
  echo "!! bun not found — skipping frontend gate (install bun or use pnpm)"
fi

echo "✓ all checks passed"
