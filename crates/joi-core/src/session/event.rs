//! The events a [`super::RealtimeSession`] emits and the UI-facing events the manager fans out.
//!
//! Provider adapters bridge their wire messages into one **ordered** [`SessionEvent`] stream
//! (transcript-before-turn-end), delivered over an owned [`EventReceiver`] taken once after
//! `connect` (SPEC §4). The [`crate::manager::SessionManager`] maps these to [`UiEvent`]s — the
//! serializable shape mirrored by the frontend (SPEC §11.3).

use serde::{Deserialize, Serialize};

use crate::error::SessionError;
use crate::history::HistoryMeta;
use crate::metrics::MetricsSnapshot;
use crate::tools::ToolCallId;

/// Owned receiver for a session's event stream (SPEC §4 — taken once via `take_events`).
pub type EventReceiver = tokio::sync::mpsc::Receiver<SessionEvent>;
/// Sender half adapters push [`SessionEvent`]s into.
pub type EventSender = tokio::sync::mpsc::Sender<SessionEvent>;

/// Who is speaking in a transcript line (SPEC §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Speaker {
    /// The human user.
    User,
    /// The agent.
    Agent,
}

/// Turn-taking boundary events (SPEC §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEvent {
    /// A side began a turn.
    TurnStarted,
    /// A turn finished normally.
    TurnComplete,
    /// The agent's turn was interrupted by barge-in (FR-2).
    Interrupted,
}

/// Why a session closed (SPEC §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The client called `close` (stop/pause).
    Client,
    /// The provider closed the socket.
    Server,
    /// Closed due to an error.
    Error,
}

/// One item in a session's ordered outbound event stream (SPEC §4).
///
/// Not serialized across IPC directly — the manager maps it to [`UiEvent`] (and routes audio over
/// the binary Channel).
#[derive(Debug)]
pub enum SessionEvent {
    /// 24 kHz mono PCM16 to play back.
    AudioOutput {
        /// Output samples.
        pcm: Vec<i16>,
    },
    /// A (possibly partial) transcript line for the terminal UI (FR-3).
    Transcript {
        /// Who spoke.
        speaker: Speaker,
        /// The text so far.
        text: String,
        /// `true` once this line is finalized (committed to history).
        final_: bool,
    },
    /// A turn-taking boundary.
    TurnEvent(TurnEvent),
    /// `[POST]` A model-emitted tool call. Unused in the MVP (SPEC §10).
    ToolCall {
        /// Provider-assigned id.
        id: ToolCallId,
        /// Tool name.
        name: String,
        /// Tool arguments.
        args: serde_json::Value,
    },
    /// A session-resumption handle for transient reconnects (SPEC §5.2).
    SessionResumptionUpdate {
        /// Opaque provider handle.
        handle: String,
    },
    /// A provider/transport error.
    Error(SessionError),
    /// The session closed.
    Closed {
        /// Why it closed.
        reason: CloseReason,
    },
}

/// High-level lifecycle/UI state surfaced to the user at all times (FR-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum AppState {
    /// No session, no cost.
    Stopped,
    /// Establishing the connection.
    Connecting,
    /// Connected, waiting for the user to speak.
    Listening,
    /// The model is processing.
    Thinking,
    /// The model is speaking.
    Speaking,
    /// Transiently reconnecting (SPEC §5.2).
    Reconnecting,
    /// An error state requiring user attention.
    Error,
}

/// Connection status detail surfaced via the `connection` event (SPEC §11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum ConnectionStatus {
    /// Disconnected.
    Disconnected,
    /// Connecting.
    Connecting,
    /// Connected.
    Connected,
    /// Reconnecting.
    Reconnecting,
}

/// UI-facing event emitted to the webview (SPEC §11.3). Audio is **not** here — it streams over the
/// binary `tauri::ipc::Channel` (SPEC §11.2).
///
/// Not `Eq`: the `Metrics` payload carries `f64` rates (`MetricsSnapshot`), which have no total
/// equality. `PartialEq` is kept for tests and change-detection.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum UiEvent {
    /// Lifecycle/UI state change (FR-4).
    State {
        /// The new state.
        state: AppState,
    },
    /// A transcript line for the terminal (SPEC §8).
    Transcript {
        /// Who spoke.
        speaker: Speaker,
        /// The text.
        text: String,
        /// Whether this line is finalized. Serializes as `final` (m-4 raw-keyword dodge).
        #[serde(rename = "final")]
        final_: bool,
    },
    /// Connection status update.
    Connection {
        /// The status.
        status: ConnectionStatus,
        /// Optional human-readable detail.
        detail: Option<String>,
    },
    /// History changed (append/prune) — drives the history meta in the UI.
    History(HistoryMeta),
    /// A throughput sample (up/down kbit/s + tokens/s), emitted roughly once a second while a
    /// session is live so the UI can render a live bandwidth/generation-speed indicator.
    Metrics(MetricsSnapshot),
    /// A surfaced error.
    Error {
        /// Short machine-ish kind.
        kind: String,
        /// Human-readable message.
        message: String,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn transcript_final_serializes_as_final() {
        let ev = UiEvent::Transcript {
            speaker: Speaker::Agent,
            text: "hi".to_string(),
            final_: true,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "transcript");
        assert_eq!(json["final"], true);
        assert_eq!(json["speaker"], "agent");
    }

    #[test]
    fn ui_event_roundtrips() {
        let ev = UiEvent::State {
            state: AppState::Listening,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: UiEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }
}
