//! Layered configuration (defaults → YAML file → `JOI_` env), loaded once at startup.
//!
//! Precedence, lowest to highest (PLAN §4.1): built-in [`Config::default`], a YAML file, then
//! `JOI_`-prefixed environment variables (nested via `__`) — **env always wins over the file**. CLI
//! flags (`--config`/`--log`) are applied by the binary *before* this loader runs. Every config
//! value can therefore be set in the YAML file or via the environment.
//!
//! On startup the binary writes a defaults file to the config path if none exists
//! ([`Config::write_default_if_missing`]), so users have a documented YAML to edit.
//!
//! The provider API key may be set in the file (`live_api.gemini.api_key`) or, preferably, via the
//! `GEMINI_API_KEY` (or `JOI_LIVE_API__GEMINI__API_KEY`) environment variable — env wins. It is held
//! as a redacting [`ApiKey`] so it never leaks into logs.
//!
//! `joi-core` is the single source of truth for XDG paths (PLAN §4.2): the binary must pass these
//! resolved paths in rather than re-deriving them, to avoid divergent locations.

use std::path::{Path, PathBuf};

use figment::{
    providers::{Env, Serialized},
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

/// Provider API key, settable in the YAML file or via env. Redacts in `Debug` so it can't leak into
/// logs; empty means unset. Unlike `secrecy::SecretString` it supports the serde + `Eq` derives
/// [`Config`] needs (and the key may legitimately live in config now).
#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "ApiKey(unset)"
        } else {
            "ApiKey(<redacted>)"
        })
    }
}

impl ApiKey {
    /// Wrap a raw key string.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The raw key, or `None` when unset/empty.
    #[must_use]
    pub fn get(&self) -> Option<&str> {
        (!self.0.is_empty()).then_some(self.0.as_str())
    }

    /// Whether a non-empty key is set.
    #[must_use]
    pub fn is_set(&self) -> bool {
        !self.0.is_empty()
    }
}

/// Live-API configuration: which provider to drive and its per-provider settings (SPEC §13). Only
/// `gemini` is functional in the MVP, so it is the single provider block today.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LiveApiCfg {
    /// Which live-API provider to use.
    pub provider: ProviderName,
    /// Gemini Live settings (used when `provider` is `gemini`).
    pub gemini: GeminiCfg,
    /// Cadence, in seconds, of the background reachability probe (token-free; never consumes
    /// tokens). `0` disables periodic polling — a probe still runs at startup and on demand.
    /// Defaulted so configs written before this field still parse.
    #[serde(default = "default_reachability_probe_secs")]
    pub reachability_probe_secs: u64,
}

/// Default reachability-probe cadence: 20 s — responsive enough for a live status dot without
/// chattering at the API.
fn default_reachability_probe_secs() -> u64 {
    20
}

/// Gemini Live provider settings. The API key may be set here or via the `GEMINI_API_KEY` /
/// `JOI_LIVE_API__GEMINI__API_KEY` environment variable (env wins).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GeminiCfg {
    /// Exact model id, e.g. `gemini-live-2.5-flash-native-audio`.
    pub model: String,
    /// API key. Empty = unset; prefer providing it via the environment.
    #[serde(default)]
    pub api_key: ApiKey,
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
    /// Mic capture device. `"default"` follows the OS/desktop default input device; any other value
    /// pins that exact device by name (the host device names are logged at startup), letting Joi
    /// bypass a virtual/processed default such as a PipeWire echo-cancel source.
    pub input_device: String,
    /// Playback device. `"default"` follows the OS/desktop default output device; any other value
    /// pins that exact device by name. See [`Self::input_device`].
    pub output_device: String,
    /// Acoustic echo cancellation: subtract Joi's own playback from the mic so the model doesn't
    /// hear itself (and interrupt itself) on speakers. Turn off when using headphones, or when an
    /// OS/server APM (e.g. PipeWire's echo-cancel source) already does it.
    pub echo_cancellation: bool,
    /// Noise suppression on the mic. Disable when an OS/server APM already conditions the input, to
    /// avoid double-processing.
    pub noise_suppression: bool,
    /// Automatic gain control on the mic. Disable when an OS APM does the conditioning: an AGC stage
    /// *without* a co-located echo canceller is echo-blind and will amplify residual echo during
    /// playback into false barge-ins.
    pub auto_gain: bool,
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct TerminalCfg {
    /// ANSI theme name.
    pub theme: String,
    /// Monospace font family.
    pub font: String,
    /// Scrollback line count.
    pub scrollback: u32,
    /// Background color — a hex string (`#rrggbb`) or `transparent` to inherit the terminal's own
    /// background. Honored by the TUI host (the web frontend keeps its CSS theme).
    pub background: String,
    /// Accent color as a hex string (`#rrggbb`).
    pub accent: String,
}

/// Logging settings (SPEC §15).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LoggingCfg {
    /// Filter level. `RUST_LOG` overrides this.
    pub level: LogLevel,
    /// Log file path. `None` → resolved to the XDG state dir at load time.
    pub file: Option<PathBuf>,
}

/// Native media I/O settings — the `joi-media` module's slice (audio + screen).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MediaCfg {
    /// Audio I/O.
    pub audio: AudioCfg,
    /// Screen capture.
    pub screen: ScreenCfg,
}

/// Web-frontend settings — delivered to the UI by the host; not used by the engine itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct UiCfg {
    /// Terminal appearance.
    pub terminal: TerminalCfg,
}

/// The complete, validated application configuration. Top-level fields are per-module sections.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Config {
    /// Live-API provider selection + per-provider settings (`joi-providers`).
    pub live_api: LiveApiCfg,
    /// History persistence (`joi-core`).
    pub history: HistoryCfg,
    /// Logging (`joi-core`).
    pub logging: LoggingCfg,
    /// Native media I/O (`joi-media`): audio + screen.
    pub media: MediaCfg,
    /// Web-frontend settings.
    pub ui: UiCfg,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            live_api: LiveApiCfg {
                provider: ProviderName::Gemini,
                gemini: GeminiCfg {
                    model: "gemini-live-2.5-flash-native-audio".to_string(),
                    api_key: ApiKey::default(),
                    voice: Some("Aoede".to_string()),
                    system_instruction: "You are Joi, a concise local voice companion.".to_string(),
                    input_transcription: true,
                    output_transcription: true,
                },
                reachability_probe_secs: default_reachability_probe_secs(),
            },
            history: HistoryCfg {
                dir: None,
                token_budget: 32_000,
            },
            logging: LoggingCfg {
                level: LogLevel::Info,
                file: None,
            },
            media: MediaCfg {
                audio: AudioCfg {
                    input_sample_rate: 16_000,
                    output_sample_rate: 24_000,
                    frame_ms: 20,
                    input_device: "default".to_string(),
                    output_device: "default".to_string(),
                    echo_cancellation: true,
                    noise_suppression: true,
                    auto_gain: true,
                },
                screen: ScreenCfg {
                    enabled: false,
                    capture_path: CapturePath::Auto,
                    fps: 1.0,
                    // Sized to the provider's per-frame video resolution. Gemini Live tiles each
                    // frame to ~768 px (one 768x768 tile / ~258 tokens); more is downsampled away.
                    max_width: 768,
                    quality: 80,
                },
            },
            ui: UiCfg {
                terminal: TerminalCfg {
                    theme: "joi-dark".to_string(),
                    font: "JetBrains Mono".to_string(),
                    scrollback: 5_000,
                    background: "transparent".to_string(),
                    accent: "#9aede4".to_string(),
                },
            },
        }
    }
}

/// XDG-resolved paths Joi uses. The binary passes these in rather than re-deriving them (m-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPaths {
    /// Default config file (`~/.config/joi/joi.yaml`).
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
            config_file: dirs.config_dir().join("joi.yaml"),
            history_dir: data_dir.join("history"),
            data_dir,
            log_dir,
        })
    }
}

impl Config {
    /// Load configuration: defaults → YAML file (if present) → `JOI_` env, then validate.
    ///
    /// `cli_path` overrides the default config-file location. If no file exists there yet, a
    /// defaults file is written first (best-effort) so the user has something to edit. Unset
    /// path-typed fields (`history.dir`, `logging.file`) are resolved against XDG locations.
    pub fn load(cli_path: Option<&Path>) -> Result<Self, ConfigError> {
        let paths = ProjectPaths::resolve()?;
        let file = cli_path.map_or_else(|| paths.config_file.clone(), Path::to_path_buf);
        if let Err(e) = Self::write_default_if_missing(&file) {
            tracing::warn!("could not write default config to {}: {e}", file.display());
        }
        let mut cfg = Self::load_from(&file, &paths)?;
        // Conventional provider env shortcuts win over the file (the nested `JOI_LIVE_API__GEMINI__*`
        // form is already handled by `load_from`'s figment env layer).
        cfg.apply_provider_env_overrides();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Overlay the conventional provider env vars `GEMINI_API_KEY` and `GEMINI_MODEL` onto
    /// `live_api.gemini.{api_key,model}`. Non-empty env values win over whatever the file set.
    fn apply_provider_env_overrides(&mut self) {
        self.apply_provider_overrides(
            std::env::var("GEMINI_API_KEY").ok(),
            std::env::var("GEMINI_MODEL").ok(),
        );
    }

    /// Pure core of [`apply_provider_env_overrides`] (env reading split out so it's testable without
    /// mutating the process environment). Non-empty values replace the current ones.
    fn apply_provider_overrides(&mut self, api_key: Option<String>, model: Option<String>) {
        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            self.live_api.gemini.api_key = ApiKey::new(key);
        }
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            self.live_api.gemini.model = model;
        }
    }

    /// Write the built-in defaults as YAML to `path` if no config file exists there yet (creating
    /// parent dirs). Gives the user a documented file to edit; never overwrites an existing one.
    /// This is the one place Joi writes config — secrets are never included (they come from the
    /// environment).
    pub fn write_default_if_missing(path: &Path) -> Result<(), ConfigError> {
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::Path {
                path: parent.to_path_buf(),
                reason: e.to_string(),
            })?;
        }
        let yaml = serde_norway::to_string(&Config::default())
            .map_err(|e| ConfigError::Load(e.to_string()))?;
        std::fs::write(path, yaml).map_err(|e| ConfigError::Path {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        tracing::info!("wrote default config to {}", path.display());
        Ok(())
    }

    /// The path-resolution + figment merge used by [`Config::load`], with paths injected so it is
    /// testable without touching the real environment.
    ///
    /// The YAML file is parsed with `serde_norway` and merged (deep, over the defaults) via figment;
    /// `JOI_` env vars are layered last and win. A missing file is treated as empty.
    pub fn load_from(file: &Path, paths: &ProjectPaths) -> Result<Self, ConfigError> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));
        if file.exists() {
            let contents = std::fs::read_to_string(file)
                .map_err(|e| ConfigError::Load(format!("{}: {e}", file.display())))?;
            let value: serde_norway::Value =
                serde_norway::from_str(&contents).map_err(|e| ConfigError::Load(e.to_string()))?;
            figment = figment.merge(Serialized::defaults(value));
        }
        let mut cfg: Config = figment
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

        if self.live_api.gemini.model.trim().is_empty() {
            return Err(invalid("live_api.gemini.model", "must not be empty"));
        }
        let audio = &self.media.audio;
        let screen = &self.media.screen;
        if audio.input_sample_rate == 0 || audio.output_sample_rate == 0 {
            return Err(invalid("media.audio.*_sample_rate", "must be > 0"));
        }
        if !(5..=60).contains(&audio.frame_ms) {
            return Err(invalid(
                "media.audio.frame_ms",
                "must be between 5 and 60 ms",
            ));
        }
        if !(screen.fps.is_finite() && screen.fps > 0.0 && screen.fps <= 60.0) {
            return Err(invalid("media.screen.fps", "must be in (0, 60]"));
        }
        if screen.max_width == 0 {
            return Err(invalid("media.screen.max_width", "must be > 0"));
        }
        if !(1..=100).contains(&screen.quality) {
            return Err(invalid("media.screen.quality", "must be between 1 and 100"));
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
            config_file: PathBuf::from("/nonexistent/joi.yaml"),
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
        // Guards config/joi.example.yaml against drift from the Config schema.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/joi.example.yaml");
        let cfg = Config::load_from(&path, &test_paths()).unwrap();
        // Assert only fields the parallel Jail tests never set via env (they mutate real process
        // env, which figment reads), to avoid cross-test races.
        assert_eq!(cfg.live_api.provider, ProviderName::Gemini);
        assert_eq!(cfg.live_api.gemini.voice.as_deref(), Some("Aoede"));
        // The template leaves the key empty — it comes from the environment.
        assert!(!cfg.live_api.gemini.api_key.is_set());
    }

    #[test]
    fn missing_file_yields_defaults_with_resolved_paths() {
        let paths = test_paths();
        let cfg = Config::load_from(Path::new("/nonexistent/joi.yaml"), &paths).unwrap();
        assert_eq!(cfg.live_api.provider, ProviderName::Gemini);
        assert_eq!(cfg.history.dir, Some(PathBuf::from("/data/history")));
        assert_eq!(cfg.logging.file, Some(PathBuf::from("/state/joi.log")));
    }

    #[test]
    fn writes_default_file_when_missing_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // A nested path exercises parent-dir creation.
        let path = dir.path().join("nested/joi.yaml");
        assert!(!path.exists());
        Config::write_default_if_missing(&path).unwrap();
        assert!(path.exists());
        // The written file round-trips back to the defaults.
        let cfg = Config::load_from(&path, &test_paths()).unwrap();
        assert_eq!(cfg.live_api.provider, ProviderName::Gemini);
        // A second call must not overwrite or error.
        Config::write_default_if_missing(&path).unwrap();
    }

    #[test]
    fn file_overrides_defaults() {
        figment::Jail::expect_with(|jail| {
            // YAML body starts at column 0 — indentation is significant.
            jail.create_file(
                "joi.yaml",
                r"
live_api:
  provider: mock
  gemini:
    model: test-model
media:
  audio:
    frame_ms: 40
",
            )?;
            let cfg = Config::load_from(Path::new("joi.yaml"), &test_paths()).unwrap();
            assert_eq!(cfg.live_api.provider, ProviderName::Mock);
            assert_eq!(cfg.live_api.gemini.model, "test-model");
            assert_eq!(cfg.media.audio.frame_ms, 40);
            // unspecified nested fields keep their defaults (deep merge)
            assert_eq!(cfg.live_api.gemini.voice.as_deref(), Some("Aoede"));
            assert_eq!(cfg.media.audio.input_sample_rate, 16_000);
            Ok(())
        });
    }

    #[test]
    fn api_key_from_file_and_nested_joi_env() {
        // In the file…
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.yaml",
                r"
live_api:
  gemini:
    api_key: file-key
",
            )?;
            let cfg = Config::load_from(Path::new("joi.yaml"), &test_paths()).unwrap();
            assert_eq!(cfg.live_api.gemini.api_key.get(), Some("file-key"));
            Ok(())
        });
        // …and the nested JOI_ env form wins over the file.
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.yaml",
                r"
live_api:
  gemini:
    api_key: file-key
",
            )?;
            jail.set_env("JOI_LIVE_API__GEMINI__API_KEY", "env-key");
            let cfg = Config::load_from(Path::new("joi.yaml"), &test_paths()).unwrap();
            assert_eq!(cfg.live_api.gemini.api_key.get(), Some("env-key"));
            Ok(())
        });
    }

    #[test]
    fn gemini_convenience_env_overrides_win() {
        // `GEMINI_API_KEY`/`GEMINI_MODEL` map onto live_api.gemini and beat the file value. Test the
        // pure core so we don't mutate the process env (which would race the parallel Jail tests).
        let mut cfg = Config::default();
        cfg.live_api.gemini.api_key = ApiKey::new("file-secret");
        cfg.live_api.gemini.model = "file-model".to_string();
        cfg.apply_provider_overrides(Some("env-secret".into()), Some("env-model".into()));
        assert_eq!(cfg.live_api.gemini.api_key.get(), Some("env-secret"));
        assert_eq!(cfg.live_api.gemini.model, "env-model");
        // Empty/absent env values leave the existing value untouched.
        cfg.apply_provider_overrides(Some(String::new()), None);
        assert_eq!(cfg.live_api.gemini.api_key.get(), Some("env-secret"));
    }

    #[test]
    fn api_key_redacts_in_debug() {
        let rendered = format!("{:?}", ApiKey::new("super-secret-key-987"));
        assert!(
            !rendered.contains("super-secret-key-987"),
            "Debug leaked: {rendered}"
        );
        assert_eq!(format!("{:?}", ApiKey::default()), "ApiKey(unset)");
    }

    #[test]
    fn env_overrides_file() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.yaml",
                r"
live_api:
  provider: mock
  gemini:
    model: from-file
",
            )?;
            jail.set_env("JOI_LIVE_API__GEMINI__MODEL", "from-env");
            jail.set_env("JOI_MEDIA__AUDIO__FRAME_MS", "30");
            let cfg = Config::load_from(Path::new("joi.yaml"), &test_paths()).unwrap();
            assert_eq!(cfg.live_api.gemini.model, "from-env");
            assert_eq!(cfg.media.audio.frame_ms, 30);
            Ok(())
        });
    }

    #[test]
    fn invalid_value_is_rejected() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.yaml",
                r"
media:
  audio:
    frame_ms: 500
",
            )?;
            let err = Config::load_from(Path::new("joi.yaml"), &test_paths()).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }));
            Ok(())
        });
    }
}
