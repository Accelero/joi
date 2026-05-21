//! The API-key port.
//!
//! The key is a [`secrecy::SecretString`] whose `Debug` redacts, so it **cannot** be formatted
//! into a log by accident — the construction-safe control from PLAN §5 (M-6). The production
//! adapter is the OS keychain (`src-tauri`); this module provides the trait and an in-memory
//! implementation for tests.

use async_trait::async_trait;
use secrecy::SecretString;
use tokio::sync::Mutex;

use crate::error::SecretError;

/// Read/write access to the provider API key. The key never travels through [`crate::Config`]
/// (SPEC SEC-5).
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// The stored key, or `None` if the user has not entered one yet.
    async fn get_api_key(&self) -> Result<Option<SecretString>, SecretError>;

    /// Store (or replace) the key.
    async fn set_api_key(&self, key: SecretString) -> Result<(), SecretError>;

    /// Whether a key is present, without exposing it. Used by the `has_api_key` IPC command.
    async fn has_api_key(&self) -> Result<bool, SecretError> {
        Ok(self.get_api_key().await?.is_some())
    }
}

/// In-memory [`SecretStore`] for tests. Never used in production (no persistence).
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    key: Mutex<Option<String>>,
}

impl InMemorySecretStore {
    /// An empty store with no key set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SecretStore for InMemorySecretStore {
    async fn get_api_key(&self) -> Result<Option<SecretString>, SecretError> {
        Ok(self.key.lock().await.clone().map(SecretString::from))
    }

    async fn set_api_key(&self, key: SecretString) -> Result<(), SecretError> {
        use secrecy::ExposeSecret;
        *self.key.lock().await = Some(key.expose_secret().to_string());
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let store = InMemorySecretStore::new();
        assert!(!store.has_api_key().await.unwrap());
        store
            .set_api_key(SecretString::from("abc123"))
            .await
            .unwrap();
        assert!(store.has_api_key().await.unwrap());
        let got = store.get_api_key().await.unwrap().unwrap();
        assert_eq!(got.expose_secret(), "abc123");
    }

    #[test]
    fn debug_redacts_the_key() {
        let secret = SecretString::from("super-secret-key-987");
        let rendered = format!("{secret:?}");
        assert!(
            !rendered.contains("super-secret-key-987"),
            "Debug leaked the secret: {rendered}"
        );
    }
}
