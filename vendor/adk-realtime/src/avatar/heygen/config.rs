//! HeyGen provider configuration.

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// Video quality setting for HeyGen streaming sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeyGenQuality {
    /// Low quality (faster, less bandwidth).
    Low,
    /// Medium quality (balanced).
    Medium,
    /// High quality (best visual fidelity).
    High,
}

/// Configuration for the HeyGen avatar provider.
///
/// # Example
///
/// ```rust,ignore
/// use adk_realtime::avatar::heygen::{HeyGenConfig, HeyGenQuality};
///
/// let config = HeyGenConfig::new("your-api-key")
///     .with_quality(HeyGenQuality::High)
///     .with_push_to_talk(false)
///     .with_idle_timeout(300);
/// ```
pub struct HeyGenConfig {
    /// HeyGen API key.
    pub api_key: SecretString,
    /// HeyGen API base URL (default: `https://api.heygen.com`).
    pub api_base_url: String,
    /// Video quality setting.
    pub quality: HeyGenQuality,
    /// Whether to enable push-to-talk mode.
    pub push_to_talk: bool,
    /// Idle timeout in seconds before the session auto-closes.
    pub idle_timeout_secs: Option<u32>,
}

impl HeyGenConfig {
    /// Create a new `HeyGenConfig` with the given API key and sensible defaults.
    ///
    /// Defaults:
    /// - `api_base_url`: `https://api.heygen.com`
    /// - `quality`: `High`
    /// - `push_to_talk`: `false`
    /// - `idle_timeout_secs`: `None`
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: SecretString::from(api_key.into()),
            api_base_url: "https://api.heygen.com".to_string(),
            quality: HeyGenQuality::High,
            push_to_talk: false,
            idle_timeout_secs: None,
        }
    }

    /// Set the video quality.
    pub fn with_quality(mut self, quality: HeyGenQuality) -> Self {
        self.quality = quality;
        self
    }

    /// Enable or disable push-to-talk mode.
    pub fn with_push_to_talk(mut self, enabled: bool) -> Self {
        self.push_to_talk = enabled;
        self
    }

    /// Set the idle timeout in seconds.
    pub fn with_idle_timeout(mut self, secs: u32) -> Self {
        self.idle_timeout_secs = Some(secs);
        self
    }

    /// Set a custom API base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }
}

// Custom Debug that redacts the API key.
impl std::fmt::Debug for HeyGenConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeyGenConfig")
            .field("api_key", &"[REDACTED]")
            .field("api_base_url", &self.api_base_url)
            .field("quality", &self.quality)
            .field("push_to_talk", &self.push_to_talk)
            .field("idle_timeout_secs", &self.idle_timeout_secs)
            .finish()
    }
}
