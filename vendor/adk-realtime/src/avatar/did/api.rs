//! D-ID REST API request/response types.
//!
//! These types model the D-ID agent session API endpoints used for
//! session lifecycle management and WebRTC signaling.

use serde::{Deserialize, Serialize};

/// Request body for `POST /agents/{agent_id}/chat`.
///
/// Creates a new D-ID agent chat session with WebRTC signaling.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateSessionRequest {
    /// URL to the avatar source image or video.
    pub source_url: String,
    /// Optional custom LLM configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm: Option<super::config::DIDLlmConfig>,
    /// Optional knowledge base ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knowledge_id: Option<String>,
}

/// Response from `POST /agents/{agent_id}/chat`.
///
/// Contains the WebRTC SDP offer and ICE servers for establishing
/// the peer connection.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateSessionResponse {
    /// Chat identifier.
    pub id: String,
    /// Session identifier used for subsequent API calls.
    pub session_id: String,
    /// SDP offer from D-ID's WebRTC peer.
    pub offer: String,
    /// ICE server configurations for NAT traversal.
    pub ice_servers: Vec<super::super::types::IceServer>,
}

/// Request body for `POST /agents/{agent_id}/chat/{session_id}/sdp`.
///
/// Sends the local SDP answer back to D-ID to complete WebRTC negotiation.
#[allow(dead_code)]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SdpAnswerRequest {
    /// SDP answer string.
    pub answer: String,
}
