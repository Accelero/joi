//! HeyGen REST API request/response types.
//!
//! These types model the HeyGen streaming API endpoints used for
//! session lifecycle management.

use serde::{Deserialize, Serialize};

use super::config::HeyGenQuality;

/// Request body for `POST /v1/streaming.new`.
///
/// Creates a new HeyGen streaming avatar session.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateSessionRequest {
    /// HeyGen avatar identifier.
    pub avatar_id: String,
    /// Video quality setting.
    pub quality: HeyGenQuality,
    /// API version (typically `"v2"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Top-level response from `POST /v1/streaming.new`.
#[derive(Debug, Deserialize)]
pub(crate) struct CreateSessionResponse {
    /// Nested session data.
    pub data: CreateSessionData,
}

/// Session data returned inside [`CreateSessionResponse`].
#[derive(Debug, Deserialize)]
pub(crate) struct CreateSessionData {
    /// Provider-assigned session identifier.
    pub session_id: String,
    /// LiveKit access token for joining the room.
    pub access_token: String,
    /// LiveKit server URL.
    pub url: String,
}

/// Request body for `POST /v1/streaming.stop`.
///
/// Terminates an active HeyGen streaming session.
#[derive(Debug, Serialize)]
pub(crate) struct StopSessionRequest {
    /// Session identifier to stop.
    pub session_id: String,
}

/// Request body for `POST /v1/streaming.task` (keep-alive).
///
/// Sends a keep-alive signal to prevent idle timeout.
#[derive(Debug, Serialize)]
pub(crate) struct TaskRequest {
    /// Session identifier.
    pub session_id: String,
    /// Task text (empty string for keep-alive).
    pub text: String,
}
