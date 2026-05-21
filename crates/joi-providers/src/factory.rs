//! Provider selection — the composition-root helper that turns [`Config`] + the API key into a
//! [`SessionFactory`] (PLAN §1: "only the composition root wires them together; the manager never
//! names a concrete provider").
//!
//! This lives here, not in `joi-core` (which must stay provider-agnostic) and not duplicated in the
//! Tauri binary (which stays a thin shell). It is pure and fully unit-testable without a webview.

use joi_core::config::{Config, ProviderName};
use joi_core::manager::SessionFactory;
use joi_core::session::RealtimeSession;
use secrecy::SecretString;

/// Why a [`SessionFactory`] could not be built.
#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    /// The gemini provider was selected but no API key was available (SPEC SEC-5).
    #[error("no API key available for the gemini provider")]
    MissingApiKey,
    /// The selected provider's Cargo feature is not compiled in.
    #[error("provider '{0}' is not compiled in (feature disabled)")]
    ProviderDisabled(&'static str),
}

/// Build the [`SessionFactory`] for `config.provider.name`, capturing `api_key` for providers that
/// need it. The returned factory creates a fresh, unconnected session per call (start/resume).
///
/// `api_key` is taken from the [`joi_core::secrets::SecretStore`] by the caller — it never travels
/// through [`Config`].
pub fn build_session_factory(
    config: &Config,
    api_key: Option<SecretString>,
) -> Result<Box<dyn SessionFactory>, FactoryError> {
    match config.provider.name {
        ProviderName::Gemini => {
            #[cfg(feature = "gemini")]
            {
                let key = api_key.ok_or(FactoryError::MissingApiKey)?;
                Ok(Box::new(move || {
                    Box::new(crate::gemini::GeminiAdapter::new(key.clone()))
                        as Box<dyn RealtimeSession>
                }))
            }
            #[cfg(not(feature = "gemini"))]
            {
                drop(api_key);
                Err(FactoryError::ProviderDisabled("gemini"))
            }
        }
        ProviderName::Openai => {
            drop(api_key);
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
            drop(api_key);
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn config_with(name: ProviderName) -> Config {
        let mut c = Config::default();
        c.provider.name = name;
        c
    }

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_factory_creates_a_connectable_session() {
        let factory = build_session_factory(&config_with(ProviderName::Mock), None).unwrap();
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
        assert!(build_session_factory(&config_with(ProviderName::Openai), None).is_ok());
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_without_a_key_is_an_error() {
        assert!(matches!(
            build_session_factory(&config_with(ProviderName::Gemini), None),
            Err(FactoryError::MissingApiKey)
        ));
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_with_a_key_builds() {
        let factory = build_session_factory(
            &config_with(ProviderName::Gemini),
            Some(SecretString::from("k")),
        );
        assert!(factory.is_ok());
    }
}
