//! Production [`SecretStore`]: the `GEMINI_API_KEY` env var is the persistent source (SPEC SEC-5
//! dev path), with a process-lifetime overlay backing the settings-UI `set_api_key`. OS-keychain
//! persistence is post-MVP hardening; until then the env var (e.g. a fish universal variable) is the
//! durable store and `set_api_key` lasts only for the running session.

use async_trait::async_trait;
use joi_core::error::SecretError;
use joi_core::secrets::{EnvSecretStore, SecretStore};
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::Mutex;

/// Reads the key from the environment first, then an in-memory overlay set via the UI.
pub struct EnvWithOverlayStore {
    env: EnvSecretStore,
    overlay: Mutex<Option<String>>,
}

impl EnvWithOverlayStore {
    pub fn new() -> Self {
        Self {
            env: EnvSecretStore::new(),
            overlay: Mutex::new(None),
        }
    }
}

#[async_trait]
impl SecretStore for EnvWithOverlayStore {
    async fn get_api_key(&self) -> Result<Option<SecretString>, SecretError> {
        if let Some(key) = self.env.get_api_key().await? {
            return Ok(Some(key));
        }
        Ok(self.overlay.lock().await.clone().map(SecretString::from))
    }

    async fn set_api_key(&self, key: SecretString) -> Result<(), SecretError> {
        *self.overlay.lock().await = Some(key.expose_secret().to_string());
        Ok(())
    }
}
