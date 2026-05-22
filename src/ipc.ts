/**
 * Typed IPC boundary mirroring the Rust side (SPEC §11). Payload shapes are kept in lock-step with
 * `joi-core`'s serde types (`UiEvent`, `AppState`, …) and the command table in SPEC §11.1; the
 * parity test in `ipc.test.ts` pins the JSON shape so drift is caught (PLAN §5, m-4).
 *
 * Audio and screen capture are **not** here at all: they happen entirely in Rust (joi-media, native
 * cpal/APM). This boundary is JSON commands + UiEvents only — no media ever crosses into the webview.
 */

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";

// ── Shared enums (mirror joi-core::session::event) ────────────────────────────────────────────

export type Speaker = "user" | "agent";

export type AppState =
  | "stopped"
  | "connecting"
  | "listening"
  | "thinking"
  | "speaking"
  | "reconnecting"
  | "error";

export type ConnectionStatus = "disconnected" | "connecting" | "connected" | "reconnecting";

export interface HistoryMeta {
  turns: number;
  token_estimate: number;
  budget: number;
}

// ── Backend → frontend events (SPEC §11.3) ────────────────────────────────────────────────────
// Mirrors the internally-tagged `UiEvent` enum: `{ "type": <variant>, ...fields }`.

export type UiEvent =
  | { type: "state"; state: AppState }
  | { type: "transcript"; speaker: Speaker; text: string; final: boolean }
  | { type: "connection"; status: ConnectionStatus; detail: string | null }
  | ({ type: "history" } & HistoryMeta)
  | { type: "error"; kind: string; message: string };

/** The single event channel the backend emits `UiEvent`s on. */
export const UI_EVENT = "ui_event";

/** Subscribe to backend UI events. Returns the unlisten handle. */
export function onUiEvent(handler: (event: UiEvent) => void): Promise<UnlistenFn> {
  return tauriListen<UiEvent>(UI_EVENT, (e) => handler(e.payload));
}

// ── Frontend → backend commands (SPEC §11.1) ──────────────────────────────────────────────────

// Command arg shapes are `type` aliases (not interfaces) so they satisfy Tauri's `InvokeArgs`
// (`Record<string, unknown>`) constraint.
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
  ping: () => tauriInvoke<string>("ping"),
} as const;
