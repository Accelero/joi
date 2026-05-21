//! Layered configuration (defaults → TOML file → `JOI_` env), loaded once at startup.
//!
//! Precedence, lowest to highest (PLAN §4.1): built-in [`Config::default`], a TOML file, then
//! `JOI_`-prefixed environment variables (nested via `__`). CLI flags (`--config`/`--log`) are
//! applied by the binary *before* this loader runs.
//!
//! **Secrets are never in config** (SPEC SEC-5) — the API key lives only in a
//! [`crate::secrets::SecretStore`].
//!
//! `joi-core` is the single source of truth for XDG paths (PLAN §4.2): the binary must pass these
//! resolved paths in rather than re-deriving them, to avoid divergent locations.

use std::path::{Path, PathBuf};

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Which provider adapter to drive (SPEC §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderName {
    /// Gemini Live native audio (the only functional MVP provider).
    Gemini,
    /// OpenAI Realtime — compile-only stub in the MVP (SPEC §4.4).
    Openai,
    /// Scripted mock used for tests and the M1 demo (no network).
    Mock,
}

/// How screen frames are captured (SPEC §7.3, PLAN §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CapturePath {
    /// Resolve to `webview` or `native` from the M0 spike result at runtime.
    Auto,
    /// `getDisplayMedia` inside the webview.
    Webview,
    /// Native Rust capture (`scap`/`xcap`).
    Native,
}

/// Log verbosity. `RUST_LOG` overrides this for `tracing-subscriber` (PLAN §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Only errors.
    Error,
    /// Warnings and errors.
    Warn,
    /// The default: high-level lifecycle events.
    Info,
    /// Verbose, for debugging.
    Debug,
    /// Everything, including per-frame spans.
    Trace,
}

impl LogLevel {
    /// The matching `tracing` filter directive string.
    #[must_use]
    pub fn as_directive(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// Provider + model + persona settings (SPEC §13).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderCfg {
    /// Which adapter to use.
    pub name: ProviderName,
    /// Model id, e.g. `gemini-live-2.5-flash-native-audio`.
    pub model: String,
    /// Optional named voice.
    pub voice: Option<String>,
    /// System instruction / persona seeded into every session.
    pub system_instruction: String,
    /// Request transcription of the user's audio (FR-3).
    pub input_transcription: bool,
    /// Request transcription of the agent's audio (FR-3).
    pub output_transcription: bool,
}

/// Audio I/O settings (SPEC §7.1/7.2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AudioCfg {
    /// Sample rate sent to the provider (Hz). Gemini expects 16 kHz mono.
    pub input_sample_rate: u32,
    /// Sample rate received from the provider (Hz). Gemini emits 24 kHz mono.
    pub output_sample_rate: u32,
    /// Mic frame size in milliseconds (20 ms = 320 samples at 16 kHz).
    pub frame_ms: u32,
    /// Input device name, or `default`.
    pub input_device: String,
    /// Output device name, or `default`.
    pub output_device: String,
}

/// Screen-capture settings (SPEC §7.3, FR-11).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ScreenCfg {
    /// Whether screen sharing is enabled at all.
    pub enabled: bool,
    /// Capture pipeline selection.
    pub capture_path: CapturePath,
    /// Frames per second sent to the model.
    pub fps: f32,
    /// Resolution ceiling (longest edge, pixels).
    pub max_width: u32,
    /// JPEG/WebP encode quality, 1–100.
    pub quality: u8,
}

/// History persistence settings (SPEC §6).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HistoryCfg {
    /// Directory for the history file. `None` → resolved to the XDG data dir at load time.
    pub dir: Option<PathBuf>,
    /// Approximate token budget bounding persisted history (SPEC §6.2).
    ///
    /// This is the **Live session input limit**, much smaller than the underlying text model's
    /// 1M-class context window — do not copy that number. Confirm against the model card in M2/M3.
    pub token_budget: u32,
}

/// Terminal UI settings (SPEC §8, §13).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TerminalCfg {
    /// ANSI theme name.
    pub theme: String,
    /// Monospace font family.
    pub font: String,
    /// Scrollback line count.
    pub scrollback: u32,
}

/// Logging settings (SPEC §15).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LoggingCfg {
    /// Filter level. `RUST_LOG` overrides this.
    pub level: LogLevel,
    /// Log file path. `None` → resolved to the XDG state dir at load time.
    pub file: Option<PathBuf>,
}

/// The complete, validated application configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Config {
    /// Provider + model + persona.
    pub provider: ProviderCfg,
    /// Audio I/O.
    pub audio: AudioCfg,
    /// Screen capture.
    pub screen: ScreenCfg,
    /// History persistence.
    pub history: HistoryCfg,
    /// Terminal UI.
    pub terminal: TerminalCfg,
    /// Logging.
    pub logging: LoggingCfg,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderCfg {
                name: ProviderName::Gemini,
                model: "gemini-live-2.5-flash-native-audio".to_string(),
                voice: Some("Aoede".to_string()),
                system_instruction: "You are Joi, a concise local voice companion.".to_string(),
                input_transcription: true,
                output_transcription: true,
            },
            audio: AudioCfg {
                input_sample_rate: 16_000,
                output_sample_rate: 24_000,
                frame_ms: 20,
                input_device: "default".to_string(),
                output_device: "default".to_string(),
            },
            screen: ScreenCfg {
                enabled: false,
                capture_path: CapturePath::Auto,
                fps: 1.0,
                // Sized to the provider's per-frame video resolution. Gemini Live tiles each frame
                // to ~768 px (one 768x768 tile / ~258 tokens); sending more is downsampled away.
                max_width: 768,
                quality: 80,
            },
            history: HistoryCfg {
                dir: None,
                token_budget: 32_000,
            },
            terminal: TerminalCfg {
                theme: "joi-dark".to_string(),
                font: "JetBrains Mono".to_string(),
                scrollback: 5_000,
            },
            logging: LoggingCfg {
                level: LogLevel::Info,
                file: None,
            },
        }
    }
}

/// XDG-resolved paths Joi uses. The binary passes these in rather than re-deriving them (m-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPaths {
    /// Default config file (`~/.config/joi/joi.toml`).
    pub config_file: PathBuf,
    /// Data directory (`~/.local/share/joi/`).
    pub data_dir: PathBuf,
    /// History directory (`<data_dir>/history`).
    pub history_dir: PathBuf,
    /// Log/state directory (`~/.local/state/joi/`).
    pub log_dir: PathBuf,
}

impl ProjectPaths {
    /// Resolve the standard XDG locations for Joi.
    pub fn resolve() -> Result<Self, ConfigError> {
        let dirs =
            directories::ProjectDirs::from("", "", "joi").ok_or_else(|| ConfigError::Path {
                path: PathBuf::from("$HOME"),
                reason: "no valid home directory for XDG path resolution".to_string(),
            })?;
        let data_dir = dirs.data_dir().to_path_buf();
        // state_dir is Some on Linux; fall back to data_dir elsewhere.
        let log_dir = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_dir())
            .to_path_buf();
        Ok(Self {
            config_file: dirs.config_dir().join("joi.toml"),
            history_dir: data_dir.join("history"),
            data_dir,
            log_dir,
        })
    }
}

impl Config {
    /// Load configuration: defaults → TOML file (if present) → `JOI_` env, then validate.
    ///
    /// `cli_path` overrides the default config-file location. Unset path-typed fields
    /// (`history.dir`, `logging.file`) are resolved against XDG locations.
    pub fn load(cli_path: Option<&Path>) -> Result<Self, ConfigError> {
        let paths = ProjectPaths::resolve()?;
        let file = cli_path.map_or_else(|| paths.config_file.clone(), Path::to_path_buf);
        Self::load_from(&file, &paths)
    }

    /// The path-resolution + figment merge used by [`Config::load`], with paths injected so it is
    /// testable without touching the real environment.
    pub fn load_from(file: &Path, paths: &ProjectPaths) -> Result<Self, ConfigError> {
        let mut cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(file))
            .merge(Env::prefixed("JOI_").split("__"))
            .extract()
            .map_err(|e| ConfigError::Load(e.to_string()))?;

        if cfg.history.dir.is_none() {
            cfg.history.dir = Some(paths.history_dir.clone());
        }
        if cfg.logging.file.is_none() {
            cfg.logging.file = Some(paths.log_dir.join("joi.log"));
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject out-of-range values and bad enums before they reach the rest of the system.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |field: &str, reason: &str| ConfigError::Invalid {
            field: field.to_string(),
            reason: reason.to_string(),
        };

        if self.provider.model.trim().is_empty() {
            return Err(invalid("provider.model", "must not be empty"));
        }
        if self.audio.input_sample_rate == 0 || self.audio.output_sample_rate == 0 {
            return Err(invalid("audio.*_sample_rate", "must be > 0"));
        }
        if !(5..=60).contains(&self.audio.frame_ms) {
            return Err(invalid("audio.frame_ms", "must be between 5 and 60 ms"));
        }
        if !(self.screen.fps.is_finite() && self.screen.fps > 0.0 && self.screen.fps <= 60.0) {
            return Err(invalid("screen.fps", "must be in (0, 60]"));
        }
        if self.screen.max_width == 0 {
            return Err(invalid("screen.max_width", "must be > 0"));
        }
        if !(1..=100).contains(&self.screen.quality) {
            return Err(invalid("screen.quality", "must be between 1 and 100"));
        }
        if self.history.token_budget < 1_000 {
            return Err(invalid("history.token_budget", "must be at least 1000"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::result_large_err)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn test_paths() -> ProjectPaths {
        ProjectPaths {
            config_file: PathBuf::from("/nonexistent/joi.toml"),
            data_dir: PathBuf::from("/data"),
            history_dir: PathBuf::from("/data/history"),
            log_dir: PathBuf::from("/state"),
        }
    }

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn example_config_loads_and_validates() {
        // Guards config/joi.example.toml against drift from the Config schema.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/joi.example.toml");
        let cfg = Config::load_from(&path, &test_paths()).unwrap();
        // Assert only fields the parallel Jail tests never set via env (they mutate real process
        // env, which figment reads), to avoid cross-test races.
        assert_eq!(cfg.provider.name, ProviderName::Gemini);
        assert_eq!(cfg.provider.voice.as_deref(), Some("Aoede"));
    }

    #[test]
    fn missing_file_yields_defaults_with_resolved_paths() {
        let paths = test_paths();
        let cfg = Config::load_from(Path::new("/nonexistent/joi.toml"), &paths).unwrap();
        assert_eq!(cfg.provider.name, ProviderName::Gemini);
        assert_eq!(cfg.history.dir, Some(PathBuf::from("/data/history")));
        assert_eq!(cfg.logging.file, Some(PathBuf::from("/state/joi.log")));
    }

    #[test]
    fn file_overrides_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.toml",
                r#"
                [provider]
                name = "mock"
                model = "test-model"
                system_instruction = "hi"
                input_transcription = false
                output_transcription = false

                [audio]
                frame_ms = 40
                "#,
            )?;
            let cfg = Config::load_from(Path::new("joi.toml"), &test_paths()).unwrap();
            assert_eq!(cfg.provider.name, ProviderName::Mock);
            assert_eq!(cfg.provider.model, "test-model");
            assert_eq!(cfg.audio.frame_ms, 40);
            // unspecified fields keep their defaults
            assert_eq!(cfg.audio.input_sample_rate, 16_000);
            Ok(())
        });
    }

    #[test]
    fn env_overrides_file() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.toml",
                r#"
                [provider]
                name = "mock"
                model = "from-file"
                system_instruction = "hi"
                input_transcription = false
                output_transcription = false
                "#,
            )?;
            jail.set_env("JOI_PROVIDER__MODEL", "from-env");
            jail.set_env("JOI_AUDIO__FRAME_MS", "30");
            let cfg = Config::load_from(Path::new("joi.toml"), &test_paths()).unwrap();
            assert_eq!(cfg.provider.model, "from-env");
            assert_eq!(cfg.audio.frame_ms, 30);
            Ok(())
        });
    }

    #[test]
    fn invalid_value_is_rejected() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.toml",
                r"
                [audio]
                frame_ms = 500
                ",
            )?;
            let err = Config::load_from(Path::new("joi.toml"), &test_paths()).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }));
            Ok(())
        });
    }
}
