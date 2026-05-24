//! Provider selection — the composition-root helper that turns [`Config`] + the API key into a
//! [`SessionFactory`] (PLAN §1: "only the composition root wires them together; the manager never
//! names a concrete provider").
//!
//! This lives here, not in `joi-core` (which must stay provider-agnostic) and not duplicated in the
//! Tauri binary (which stays a thin shell). It is pure and fully unit-testable without a webview.

use std::sync::Arc;

use joi_core::config::{Config, ProviderName};
use joi_core::connectivity::ConnectivityProbe;
use joi_core::manager::SessionFactory;
use joi_core::session::RealtimeSession;
use secrecy::SecretString;

/// Why a [`SessionFactory`] could not be built.
#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    /// The gemini provider was selected but `live_api.gemini.api_key` is unset (file + env both
    /// empty).
    #[error("no API key for the gemini provider — set GEMINI_API_KEY or live_api.gemini.api_key")]
    MissingApiKey,
    /// The selected provider's Cargo feature is not compiled in.
    #[error("provider '{0}' is not compiled in (feature disabled)")]
    ProviderDisabled(&'static str),
}

/// Build the [`SessionFactory`] for `config.live_api.provider`. The Gemini key is read from
/// `config.live_api.gemini.api_key` (populated from the file or the environment by the config
/// loader). The returned factory creates a fresh, unconnected session per call (start/resume).
pub fn build_session_factory(config: &Config) -> Result<Box<dyn SessionFactory>, FactoryError> {
    match config.live_api.provider {
        ProviderName::Gemini => {
            #[cfg(feature = "gemini")]
            {
                let key = config
                    .live_api
                    .gemini
                    .api_key
                    .get()
                    .ok_or(FactoryError::MissingApiKey)?;
                let key = SecretString::from(key.to_owned());
                Ok(Box::new(move || {
                    Box::new(crate::gemini::GeminiAdapter::new(key.clone()))
                        as Box<dyn RealtimeSession>
                }))
            }
            #[cfg(not(feature = "gemini"))]
            {
                Err(FactoryError::ProviderDisabled("gemini"))
            }
        }
        ProviderName::Openai => {
            #[cfg(feature = "openai")]
            {
                Ok(Box::new(|| {
                    Box::new(crate::openai::OpenAIAdapter::new()) as Box<dyn RealtimeSession>
                }))
            }
            #[cfg(not(feature = "openai"))]
            {
                Err(FactoryError::ProviderDisabled("openai"))
            }
        }
        ProviderName::Mock => {
            #[cfg(feature = "mock")]
            {
                Ok(Box::new(|| {
                    Box::new(crate::mock::MockSession::new()) as Box<dyn RealtimeSession>
                }))
            }
            #[cfg(not(feature = "mock"))]
            {
                Err(FactoryError::ProviderDisabled("mock"))
            }
        }
    }
}

/// Build the token-free reachability probe for `config.live_api.provider`, or `None` when the
/// provider has no probe or isn't usable (e.g. Gemini without a key). Like [`build_session_factory`]
/// this is the composition root's job, so the engine never names a provider. The returned probe is
/// driven by the [`SessionManager`](joi_core::manager::SessionManager)'s reachability monitor.
#[must_use]
pub fn build_connectivity_probe(config: &Config) -> Option<Arc<dyn ConnectivityProbe>> {
    match config.live_api.provider {
        ProviderName::Gemini => {
            #[cfg(feature = "gemini")]
            {
                let key = config.live_api.gemini.api_key.get()?;
                let key = SecretString::from(key.to_owned());
                Some(Arc::new(crate::gemini::GeminiProbe::new(key)))
            }
            #[cfg(not(feature = "gemini"))]
            {
                None
            }
        }
        // No reachability probe for the OpenAI stub or the mock provider.
        ProviderName::Openai | ProviderName::Mock => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn config_with(provider: ProviderName) -> Config {
        let mut c = Config::default();
        c.live_api.provider = provider;
        // Default gemini.api_key is empty; tests that need a key set it explicitly.
        c
    }

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_factory_creates_a_connectable_session() {
        let factory = build_session_factory(&config_with(ProviderName::Mock)).unwrap();
        let mut session = factory.create();
        // The mock connects with no network — proves the factory wires a usable session.
        session
            .connect(joi_core::session::SessionConfig::from_config(
                &config_with(ProviderName::Mock),
                Vec::new(),
                None,
            ))
            .await
            .unwrap();
    }

    #[cfg(feature = "openai")]
    #[test]
    fn openai_factory_builds_without_a_key() {
        assert!(build_session_factory(&config_with(ProviderName::Openai)).is_ok());
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_without_a_key_is_an_error() {
        // Empty api_key (no file/env value) → MissingApiKey.
        assert!(matches!(
            build_session_factory(&config_with(ProviderName::Gemini)),
            Err(FactoryError::MissingApiKey)
        ));
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_with_a_key_builds() {
        let mut config = config_with(ProviderName::Gemini);
        config.live_api.gemini.api_key = joi_core::config::ApiKey::new("k");
        assert!(build_session_factory(&config).is_ok());
    }
}
