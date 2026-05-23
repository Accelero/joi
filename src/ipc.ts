/**
 * Typed IPC boundary mirroring the Rust side (SPEC §11). The payload types are **generated from
 * `joi-core`'s serde types** by ts-rs (`#[ts(export)]`) into `src/bindings/` — a single source of
 * truth for Seam B. `cargo test` regenerates them and `scripts/check.sh` fails on drift; the runtime
 * parity check in `ipc.test.ts` is a second guard. This module re-exports them so the rest of the app
 * imports IPC types from one place, and adds the command wrappers (which ts-rs can't derive — the
 * `invoke` arg shapes are function parameters, not Rust structs).
 *
 * Audio and screen capture are **not** here at all: they happen entirely in Rust (joi-media, native
 * cpal/APM). This boundary is JSON commands + UiEvents only — no media ever crosses into the webview.
 */

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";

// ── Generated IPC types (from joi-core via ts-rs) ─────────────────────────────────────────────
import type { Speaker } from "./bindings/Speaker";
import type { AppState } from "./bindings/AppState";
import type { ConnectionStatus } from "./bindings/ConnectionStatus";
import type { HistoryMeta } from "./bindings/HistoryMeta";
import type { UiEvent } from "./bindings/UiEvent";
import type { TerminalCfg } from "./bindings/TerminalCfg";
import type { UiCfg } from "./bindings/UiCfg";

export type { Speaker, AppState, ConnectionStatus, HistoryMeta, UiEvent, TerminalCfg, UiCfg };

// ── Backend → frontend events (SPEC §11.3) ────────────────────────────────────────────────────

/** The single event channel the backend emits `UiEvent`s on. */
export const UI_EVENT = "ui_event";

/** Subscribe to backend UI events. Returns the unlisten handle. */
export function onUiEvent(handler: (event: UiEvent) => void): Promise<UnlistenFn> {
  return tauriListen<UiEvent>(UI_EVENT, (e) => handler(e.payload));
}

// ── Frontend → backend commands (SPEC §11.1) ──────────────────────────────────────────────────

// Command arg shapes are `type` aliases (not interfaces) so they satisfy Tauri's `InvokeArgs`
// (`Record<string, unknown>`) constraint. These are hand-written: the `invoke` argument shapes are
// Rust *function parameters*, not serde structs, so they aren't part of the generated bindings.
export type StartArgs = { resume: boolean };
export type StopArgs = { pause: boolean };
export type SendTextArgs = { text: string };
export type SetMicMutedArgs = { muted: boolean };

export interface HasApiKeyResult {
  present: boolean;
}

// Mirrors `generate_handler!` in src-tauri/src/main.rs 1:1 — keep them in sync (SPEC §11.1).
export const commands = {
  start: (args: StartArgs) => tauriInvoke<{ session_id: string }>("start", args),
  stop: (args: StopArgs) => tauriInvoke<void>("stop", args),
  sendText: (args: SendTextArgs) => tauriInvoke<void>("send_text", args),
  setMicMuted: (args: SetMicMutedArgs) => tauriInvoke<void>("set_mic_muted", args),
  startScreenshare: () => tauriInvoke<void>("start_screenshare"),
  stopScreenshare: () => tauriInvoke<void>("stop_screenshare"),
  hasApiKey: () => tauriInvoke<HasApiKeyResult>("has_api_key"),
  getUiConfig: () => tauriInvoke<UiCfg>("get_ui_config"),
  ping: () => tauriInvoke<string>("ping"),
} as const;
