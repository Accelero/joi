//! Layered configuration (defaults → JSON file → `JOI_` env), loaded once at startup.
//!
//! Precedence, lowest to highest (PLAN §6.2): built-in [`Config::default`], a JSON file
//! (`~/.joi/config.json`), then `JOI_`-prefixed environment variables (nested via `__`) — **env
//! always wins over the file** — and finally the conventional shortcuts `GEMINI_API_KEY` /
//! `GEMINI_MODEL`. CLI flags (`--config`/`--log`) are applied by the binary *before* this loader
//! runs.
//!
//! The file is **JSON** (the engine reads *and writes* it — see the runtime settings interface in
//! `doc/SETTINGS.md` — and JSON round-trips deterministically for machine writing). A pre-JSON
//! `~/.joi/config` (YAML) is migrated to `config.json` once, on first run after the upgrade.
//!
//! On startup the binary writes a defaults file to the config path if none exists
//! ([`Config::write_default_if_missing`]), so users have a starting file to edit (the annotated
//! field reference lives in `doc/CONFIG.md`). Every write goes through
//! [`crate::util::atomic_write`], so a crash can't corrupt the config, and the API key is **never**
//! written to disk (it comes from the environment).
//!
//! The provider API key may be set in the file (`live_api.gemini.api_key`) or, preferably, via the
//! `GEMINI_API_KEY` (or `JOI_LIVE_API__GEMINI__API_KEY`) environment variable — env wins. It is held
//! as a redacting [`ApiKey`] so it never leaks into logs (SEC-1).
//!
//! The system prompt / persona lives in `~/.joi/prompt.md` (bootstrapped from the configured persona
//! on first run). When that file is present and non-blank it is the authoritative
//! `live_api.gemini.system_instruction`, overriding the inline config value.
//!
//! `joi-core` is the single source of truth for paths (PLAN §6.3): the binary must pass these
//! resolved paths in rather than re-deriving them, to avoid divergent locations.

use std::path::{Path, PathBuf};

use figment::{
    providers::{Env, Serialized},
    Figment,
};
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Which provider adapter to drive (SPEC §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderName {
    /// Gemini Live native audio (the real MVP provider).
    Gemini,
    /// Scripted mock used for tests and the headless gate (no network).
    Mock,
}

/// Log verbosity. `RUST_LOG` overrides this for `tracing-subscriber`.
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
/// logs (SEC-1); empty means unset. Unlike `secrecy::SecretString` it supports the serde + `Eq`
/// derives [`Config`] needs (and the key may legitimately live in config now).
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

/// Live-API configuration: which provider to drive and its per-provider settings. Only `gemini` is
/// a real provider; `mock` is for tests/headless. Gemini is the single provider block today.
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

impl LiveApiCfg {
    /// The history re-seed budget for the **active** provider — the Live session's input window,
    /// which is provider/model-dependent (hence it lives with the provider, not under `history`).
    /// Resolved here so the provider-agnostic [`crate::manager::SessionManager`] never reads a
    /// concrete provider's config field.
    #[must_use]
    pub fn token_budget(&self) -> u32 {
        match self.provider {
            // Mock is a scripted test double with no real context window; it reuses the Gemini
            // block's value (the only provider block today) — its size is immaterial to the mock.
            ProviderName::Gemini | ProviderName::Mock => self.gemini.token_budget,
        }
    }
}

/// Gemini Live provider settings. The API key may be set here or via the `GEMINI_API_KEY` /
/// `JOI_LIVE_API__GEMINI__API_KEY` environment variable (env wins).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GeminiCfg {
    /// Bare model name of a Live (`bidiGenerateContent`) model your key can access, e.g.
    /// `gemini-3.1-flash-live-preview` — the **simple name only**, with no `models/` prefix (the
    /// Gemini adapter qualifies it for the wire). **Required — Joi ships no default**; set it in the
    /// config file or via `GEMINI_MODEL`. Loading fails with a clear error when it is empty or
    /// prefixed. (Note: being listed by `models.list` is not sufficient — the model must also
    /// support the Live endpoint.)
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
    /// Enable the provider's server-side context-window compression (a sliding window that
    /// truncates/summarizes the oldest in-session turns once the live context window nears full).
    /// With it on, a session is no longer capped at the provider's default duration limits (15 min
    /// audio-only / 2 min audio+video) and can run indefinitely. Joi persists every turn to disk
    /// regardless, so this only bounds the *live* in-session context, never the stored history.
    /// Defaulted so configs written before this field still parse.
    #[serde(default = "default_context_window_compression")]
    pub context_window_compression: bool,
    /// Approximate token budget for re-seeding prior history into a new session, in tokens.
    ///
    /// This is the **Live session input window** for *this provider/model* — provider-dependent
    /// (e.g. ~128k for Gemini native-audio models, ~32k for half-cascade), which is why it lives
    /// with the provider rather than under `history`. It is **not** the underlying text model's
    /// 1M-class window — do not copy that number. Min 1000. Defaulted so older configs still parse.
    #[serde(default = "default_token_budget")]
    pub token_budget: u32,
}

/// Default for [`GeminiCfg::context_window_compression`]: on — long voice sessions are the common
/// case and the provider's default duration caps are otherwise a hard wall.
fn default_context_window_compression() -> bool {
    true
}

/// The default persona, shipped into `~/.joi/prompt.md` on first run and used as the inline
/// fallback. Tuned for a voice assistant: lead with the answer, no filler, and — deliberately — no
/// conversational hooks ("would you like to know more?"), so the user keeps control of where the
/// conversation goes. Edit `~/.joi/prompt.md` to change it.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are Joi, a local voice assistant. Help the user accurately and efficiently, then stop.

- Lead with the answer. Address exactly what was asked — no preamble, filler, or restating the question.
- Be brief: use only the words the request needs.
- Do not steer the conversation. Never append follow-up suggestions or invitations like \"would \
you like to know more?\" — what comes next is the user's choice.
- Be warm and respectful, but never at the cost of brevity; kindness here is clarity and not \
wasting the user's time.
- If a request is genuinely ambiguous, ask one short clarifying question; otherwise make a \
reasonable assumption and answer.
- If you don't know, say so plainly.
- You are speaking aloud: use plain, natural sentences — no markdown or formatting.";

/// Gemini's Live API context window (input token limit), in tokens — the 128k tier shared by the
/// native-audio and current `*-flash-live` models. The default re-seed budget is derived from this.
pub const GEMINI_LIVE_CONTEXT_WINDOW: u32 = 131_072;

/// Default for [`GeminiCfg::token_budget`]: **90% of the Gemini Live context window**, leaving ~10%
/// headroom for the session's streamed audio/video. With context-window compression on (the
/// default), the server slides the oldest turns out if this is ever exceeded.
fn default_token_budget() -> u32 {
    GEMINI_LIVE_CONTEXT_WINDOW * 9 / 10
}

/// Read a system-prompt file, returning its trimmed contents — or `None` when the file is absent,
/// unreadable, or blank (so the caller falls back to the inline `system_instruction`).
fn read_prompt_file(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Audio I/O settings (PLAN §7.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AudioCfg {
    /// Mic frame size in milliseconds (20 ms = 320 samples at 16 kHz).
    ///
    /// The wire sample rates themselves are not configurable — they are fixed by the provider
    /// protocol (16 kHz mono in, 24 kHz mono out; see [`crate::media::AudioFormat`]) and the
    /// pipeline resamples the device's own rate to/from them.
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
    /// Noise suppression on the mic (high-pass filter + NS). Disable when an OS/server APM already
    /// conditions the input, to avoid double-processing.
    pub noise_suppression: bool,
    /// Automatic gain control on the mic (AGC2). Disable when an OS APM does the conditioning: an AGC
    /// stage *without* a co-located echo canceller is echo-blind and will amplify residual echo
    /// during playback into false barge-ins.
    pub auto_gain: bool,
}

/// Screen-capture settings (PLAN §7.2, FR-8..10). Native capture is the only path.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ScreenCfg {
    /// Frames per second sent to the model.
    pub fps: f32,
    /// Resolution ceiling (longest edge, pixels).
    pub max_width: u32,
    /// JPEG encode quality, 1–100.
    pub quality: u8,
}

/// History persistence settings (SPEC §3.6, FR-21).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HistoryCfg {
    /// Directory holding per-session logs + the session index. `None` → resolved to
    /// `~/.joi/sessions` at load time.
    pub dir: Option<PathBuf>,
}

/// Terminal UI settings (read by `joi-tui`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TerminalCfg {
    /// Background color — a hex string (`#rrggbb`) or `transparent` to inherit the terminal's own
    /// background.
    pub background: String,
    /// Accent color as a hex string (`#rrggbb`) or a named color.
    pub accent: String,
}

/// Logging settings (SPEC §5).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LoggingCfg {
    /// Filter level. `RUST_LOG` overrides this.
    pub level: LogLevel,
    /// Log file path. `None` → resolved to `~/.joi/logs/joi.log` at load time.
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

/// Frontend appearance settings — read by the frontend (`joi-tui`); the engine does not use them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
    /// Frontend appearance.
    pub ui: UiCfg,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            live_api: LiveApiCfg {
                provider: ProviderName::Gemini,
                gemini: GeminiCfg {
                    // No default model: the user must choose one their key can access. An empty
                    // model is rejected by `validate`, so the app fails fast with guidance.
                    model: String::new(),
                    api_key: ApiKey::default(),
                    voice: Some("Aoede".to_string()),
                    system_instruction: DEFAULT_SYSTEM_PROMPT.to_string(),
                    input_transcription: true,
                    output_transcription: true,
                    context_window_compression: true,
                    token_budget: default_token_budget(),
                },
                reachability_probe_secs: default_reachability_probe_secs(),
            },
            history: HistoryCfg { dir: None },
            logging: LoggingCfg {
                level: LogLevel::Info,
                file: None,
            },
            media: MediaCfg {
                audio: AudioCfg {
                    frame_ms: 20,
                    input_device: "default".to_string(),
                    output_device: "default".to_string(),
                    echo_cancellation: true,
                    noise_suppression: true,
                    auto_gain: true,
                },
                screen: ScreenCfg {
                    fps: 1.0,
                    // Sized to the provider's per-frame video resolution. Gemini Live tiles each
                    // frame to ~768 px (one 768x768 tile / ~258 tokens); more is downsampled away.
                    max_width: 768,
                    quality: 80,
                },
            },
            ui: UiCfg {
                terminal: TerminalCfg {
                    background: "transparent".to_string(),
                    accent: "#9aede4".to_string(),
                },
            },
        }
    }
}

/// Filesystem locations Joi uses, all rooted at `~/.joi`. The binary passes these in rather than
/// re-deriving them (PLAN §6.3). This deliberately departs from XDG: everything Joi owns lives under
/// one `~/.joi` directory (config + per-session logs + logs), so it's easy to find, back up, or wipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPaths {
    /// Config file (`~/.joi/config.json`) — JSON.
    pub config_file: PathBuf,
    /// Pre-JSON config file (`~/.joi/config`, YAML). Read only to **migrate** it to `config.json`
    /// once when the latter is absent; never written.
    pub legacy_config_file: PathBuf,
    /// Joi root data directory (`~/.joi`).
    pub data_dir: PathBuf,
    /// Per-session conversation logs + the session index (`~/.joi/sessions`).
    pub sessions_dir: PathBuf,
    /// Log directory (`~/.joi/logs`).
    pub log_dir: PathBuf,
    /// System-prompt / persona file (`~/.joi/prompt.md`). When present and non-blank it is the
    /// authoritative `live_api.gemini.system_instruction` (the persona lives in a file you can edit
    /// without touching the config).
    pub prompt_file: PathBuf,
}

impl ProjectPaths {
    /// Resolve Joi's locations under `~/.joi`.
    pub fn resolve() -> Result<Self, ConfigError> {
        let home = directories::BaseDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .ok_or_else(|| ConfigError::Path {
                path: PathBuf::from("$HOME"),
                reason: "no valid home directory".to_string(),
            })?;
        let root = home.join(".joi");
        Ok(Self {
            config_file: root.join("config.json"),
            legacy_config_file: root.join("config"),
            sessions_dir: root.join("sessions"),
            log_dir: root.join("logs"),
            prompt_file: root.join("prompt.md"),
            data_dir: root,
        })
    }
}

impl Config {
    /// Load configuration: defaults → YAML file (if present) → `JOI_` env, then validate.
    ///
    /// `cli_path` overrides the default config-file location. If no file exists there yet, a
    /// defaults file is written first (best-effort) so the user has something to edit. Unset
    /// path-typed fields (`history.dir`, `logging.file`) are resolved against `~/.joi` locations.
    pub fn load(cli_path: Option<&Path>) -> Result<Self, ConfigError> {
        let paths = ProjectPaths::resolve()?;
        let file = cli_path.map_or_else(|| paths.config_file.clone(), Path::to_path_buf);
        // One-shot migration: when using the default location and only the pre-JSON `~/.joi/config`
        // (YAML) exists, convert it to `config.json` before loading. The legacy file is left in
        // place as a backup; `config.json` takes precedence from here on.
        if cli_path.is_none() && !file.exists() && paths.legacy_config_file.exists() {
            if let Err(e) = Self::migrate_legacy_to_json(&paths.legacy_config_file, &file) {
                tracing::warn!("could not migrate legacy config: {e}");
            }
        }
        if let Err(e) = Self::write_default_if_missing(&file) {
            tracing::warn!("could not write default config to {}: {e}", file.display());
        }
        let mut cfg = Self::load_from(&file, &paths)?;
        // Conventional provider env shortcuts win over the file (the nested `JOI_LIVE_API__GEMINI__*`
        // form is already handled by `load_from`'s figment env layer).
        cfg.apply_provider_env_overrides();
        // Move the persona into an editable `~/.joi/prompt.md` on first run (seeded from whatever the
        // config resolved to, so a custom persona is preserved). On later runs `load_from` reads it
        // back as the authoritative system instruction.
        if let Err(e) = Self::write_default_prompt_if_missing(
            &paths.prompt_file,
            &cfg.live_api.gemini.system_instruction,
        ) {
            tracing::warn!(
                "could not write default prompt to {}: {e}",
                paths.prompt_file.display()
            );
        }
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

    /// Write the built-in defaults as JSON to `path` if no config file exists there yet. Gives the
    /// user a starting file to edit (annotated reference: `doc/CONFIG.md`); never overwrites an
    /// existing one. The API key is never included — it comes from the environment.
    pub fn write_default_if_missing(path: &Path) -> Result<(), ConfigError> {
        if path.exists() {
            return Ok(());
        }
        Config::default().write_json(path)?;
        tracing::info!("wrote default config to {}", path.display());
        Ok(())
    }

    /// Atomically (re)write this config as pretty JSON to `path`, with the API key blanked.
    ///
    /// This is the single config write path — used by the defaults bootstrap, the legacy migration,
    /// and the runtime settings interface ([`crate::settings`]). It routes through
    /// [`crate::util::atomic_write`] (temp + fsync + rename), so a crash can never corrupt the file,
    /// and it always redacts the secret, so Joi never persists the key (SEC-1).
    pub fn write_json(&self, path: &Path) -> Result<(), ConfigError> {
        let json = self.to_redacted_json()?;
        crate::util::atomic_write(path, json.as_bytes()).map_err(|e| ConfigError::Path {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
    }

    /// Pretty-JSON serialization with the API key blanked out. The key belongs in the environment,
    /// never on disk (SEC-1), so every persisted form omits it.
    fn to_redacted_json(&self) -> Result<String, ConfigError> {
        let mut redacted = self.clone();
        redacted.live_api.gemini.api_key = ApiKey::default();
        serde_json::to_string_pretty(&redacted).map_err(|e| ConfigError::Load(e.to_string()))
    }

    /// Convert a pre-JSON `~/.joi/config` (YAML) into `config.json` (JSON), once. The legacy file is
    /// parsed and merged over the defaults (so it tolerates partial files and drops keys no longer
    /// in the schema), then written via the redacting [`write_json`](Self::write_json). A key found
    /// in the legacy file is **not** carried over — it must live in the environment.
    fn migrate_legacy_to_json(legacy: &Path, target: &Path) -> Result<(), ConfigError> {
        let contents = std::fs::read_to_string(legacy)
            .map_err(|e| ConfigError::Load(format!("{}: {e}", legacy.display())))?;
        let value: serde_norway::Value =
            serde_norway::from_str(&contents).map_err(|e| ConfigError::Load(e.to_string()))?;
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Serialized::defaults(value))
            .extract()
            .map_err(|e| ConfigError::Load(e.to_string()))?;
        if cfg.live_api.gemini.api_key.is_set() {
            tracing::warn!(
                "legacy config {} held an API key; it was NOT copied to config.json — set \
                 GEMINI_API_KEY in your environment instead",
                legacy.display()
            );
        }
        cfg.write_json(target)?;
        tracing::info!(
            "migrated legacy config {} → {}",
            legacy.display(),
            target.display()
        );
        Ok(())
    }

    /// The path-resolution + figment merge used by [`Config::load`], with paths injected so it is
    /// testable without touching the real environment.
    ///
    /// The JSON file is parsed with `serde_json` and merged (deep, over the defaults) via figment;
    /// `JOI_` env vars are layered last and win. A missing or blank file is treated as empty.
    pub fn load_from(file: &Path, paths: &ProjectPaths) -> Result<Self, ConfigError> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));
        if file.exists() {
            let contents = std::fs::read_to_string(file)
                .map_err(|e| ConfigError::Load(format!("{}: {e}", file.display())))?;
            // A blank file means "all defaults" — `serde_json` would reject the empty string.
            if !contents.trim().is_empty() {
                let value: serde_json::Value = serde_json::from_str(&contents)
                    .map_err(|e| ConfigError::Load(format!("{}: {e}", file.display())))?;
                figment = figment.merge(Serialized::defaults(value));
            }
        }
        let mut cfg: Config = figment
            .merge(Env::prefixed("JOI_").split("__"))
            .extract()
            .map_err(|e| ConfigError::Load(e.to_string()))?;

        if cfg.history.dir.is_none() {
            cfg.history.dir = Some(paths.sessions_dir.clone());
        }
        if cfg.logging.file.is_none() {
            cfg.logging.file = Some(paths.log_dir.join("joi.log"));
        }

        // A present, non-blank prompt file is the authoritative persona — it overrides the inline
        // `system_instruction` from the config/env (the one documented exception to "env wins": a
        // dedicated prompt file is a more explicit choice than an env var). Missing/blank/unreadable
        // → keep whatever the config resolved to.
        if let Some(prompt) = read_prompt_file(&paths.prompt_file) {
            cfg.live_api.gemini.system_instruction = prompt;
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// Write the current persona to `path` if no prompt file exists there yet (best-effort, creating
    /// parent dirs). Seeded from the **resolved** `system_instruction` so an existing config persona
    /// is moved into the file rather than overwritten by the built-in default. Never clobbers an
    /// existing file.
    pub fn write_default_prompt_if_missing(path: &Path, persona: &str) -> Result<(), ConfigError> {
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::Path {
                path: parent.to_path_buf(),
                reason: e.to_string(),
            })?;
        }
        std::fs::write(path, format!("{}\n", persona.trim())).map_err(|e| ConfigError::Path {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        tracing::info!("wrote default prompt to {}", path.display());
        Ok(())
    }

    /// Reject out-of-range values and bad enums before they reach the rest of the system.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |field: &str, reason: &str| ConfigError::Invalid {
            field: field.to_string(),
            reason: reason.to_string(),
        };

        let model = self.live_api.gemini.model.trim();
        if model.is_empty() {
            return Err(invalid(
                "live_api.gemini.model",
                "no model configured — set GEMINI_MODEL or live_api.gemini.model to a Live model \
                 your key can access (Joi ships no default), e.g. gemini-3.1-flash-live-preview",
            ));
        }
        if model.contains('/') {
            return Err(invalid(
                "live_api.gemini.model",
                "use the bare model name (e.g. gemini-3.1-flash-live-preview) — drop the 'models/' \
                 prefix; the Gemini adapter qualifies it for the wire",
            ));
        }
        let audio = &self.media.audio;
        let screen = &self.media.screen;
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
        if self.live_api.gemini.token_budget < 1_000 {
            return Err(invalid(
                "live_api.gemini.token_budget",
                "must be at least 1000",
            ));
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
            config_file: PathBuf::from("/nonexistent/config.json"),
            legacy_config_file: PathBuf::from("/nonexistent/config"),
            data_dir: PathBuf::from("/data"),
            sessions_dir: PathBuf::from("/data/sessions"),
            log_dir: PathBuf::from("/state"),
            prompt_file: PathBuf::from("/nonexistent/prompt.md"),
        }
    }

    #[test]
    fn prompt_file_overrides_system_instruction_when_present() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("JOI_LIVE_API__GEMINI__MODEL", "m");
            let dir = jail.directory().to_path_buf();
            let prompt = dir.join("prompt.md");
            std::fs::write(&prompt, "  You are a pirate.\n").unwrap();
            let mut paths = test_paths();
            paths.prompt_file = prompt;
            let cfg = Config::load_from(Path::new("/nonexistent/joi.json"), &paths).unwrap();
            // The file (trimmed) wins over the built-in default persona.
            assert_eq!(cfg.live_api.gemini.system_instruction, "You are a pirate.");
            Ok(())
        });
    }

    #[test]
    fn blank_or_missing_prompt_file_keeps_inline_persona() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("JOI_LIVE_API__GEMINI__MODEL", "m");
            let dir = jail.directory().to_path_buf();
            let prompt = dir.join("prompt.md");
            std::fs::write(&prompt, "   \n\t\n").unwrap(); // whitespace-only → ignored
            let mut paths = test_paths();
            paths.prompt_file = prompt;
            let cfg = Config::load_from(Path::new("/nonexistent/joi.json"), &paths).unwrap();
            assert_eq!(
                cfg.live_api.gemini.system_instruction,
                Config::default().live_api.gemini.system_instruction
            );
            Ok(())
        });
    }

    #[test]
    fn default_system_prompt_joins_continuations_cleanly() {
        // The const uses `\` line-continuations; make sure they join without stray double spaces or
        // broken words, and that the key constraints survived.
        let p = DEFAULT_SYSTEM_PROMPT;
        assert!(
            !p.contains("  "),
            "no doubled spaces from continuations: {p:?}"
        );
        assert!(p.contains("invitations like \"would you like to know more?\""));
        assert!(p.contains("not wasting the user's time"));
        assert!(p.contains("make a reasonable assumption and answer"));
        assert!(!p.ends_with('\n'), "const carries no trailing newline");
    }

    #[test]
    fn default_token_budget_is_90_percent_of_gemini_window_and_resolved_per_provider() {
        // The default re-seed budget is 90% of Gemini's Live context window, lives under the
        // provider, and is read through the provider-agnostic resolver.
        let cfg = Config::default();
        assert_eq!(
            cfg.live_api.gemini.token_budget,
            GEMINI_LIVE_CONTEXT_WINDOW * 9 / 10
        );
        assert_eq!(
            cfg.live_api.token_budget(),
            cfg.live_api.gemini.token_budget
        );
        assert_eq!(cfg.live_api.gemini.token_budget, 117_964);
    }

    #[test]
    fn default_has_no_model_and_must_be_set() {
        // Joi ships no default model — the default config is invalid until one is set explicitly.
        let mut cfg = Config::default();
        assert!(cfg.live_api.gemini.model.is_empty(), "no default model");
        assert!(
            cfg.validate().is_err(),
            "an empty model is rejected at load"
        );
        cfg.live_api.gemini.model = "gemini-3.1-flash-live-preview".to_string();
        cfg.validate().unwrap(); // valid once a bare model name is chosen
    }

    #[test]
    fn model_with_models_prefix_is_rejected() {
        // The model must be the bare name; the adapter adds the `models/` wire prefix itself.
        let mut cfg = Config::default();
        cfg.live_api.gemini.model = "models/gemini-3.1-flash-live-preview".to_string();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn example_config_loads_and_validates() {
        // Guards config/joi.example.json against drift from the Config schema. Run inside a `Jail`
        // so it shares figment's global env lock with the other env-mutating tests below — otherwise
        // a concurrent test that sets `JOI_LIVE_API__GEMINI__API_KEY` leaks into this `load_from`
        // (which always merges the `JOI_` env layer) and trips the empty-key assertion.
        figment::Jail::expect_with(|_jail| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/joi.example.json");
            let cfg = Config::load_from(&path, &test_paths()).unwrap();
            assert_eq!(cfg.live_api.provider, ProviderName::Gemini);
            assert_eq!(cfg.live_api.gemini.voice.as_deref(), Some("Aoede"));
            // The template leaves the key empty — it comes from the environment.
            assert!(!cfg.live_api.gemini.api_key.is_set());
            Ok(())
        });
    }

    #[test]
    fn missing_file_yields_defaults_with_resolved_paths() {
        // A model is required, so supply one via env (the only layer present with no file). Run in a
        // Jail so that env mutation is isolated/serialized with the other env-reading tests.
        figment::Jail::expect_with(|jail| {
            jail.set_env("JOI_LIVE_API__GEMINI__MODEL", "test-model");
            let paths = test_paths();
            let cfg = Config::load_from(Path::new("/nonexistent/joi.json"), &paths).unwrap();
            assert_eq!(cfg.live_api.provider, ProviderName::Gemini);
            assert_eq!(cfg.history.dir, Some(PathBuf::from("/data/sessions")));
            assert_eq!(cfg.logging.file, Some(PathBuf::from("/state/joi.log")));
            Ok(())
        });
    }

    #[test]
    fn writes_default_file_when_missing_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // A nested path exercises parent-dir creation.
        let path = dir.path().join("nested/config.json");
        assert!(!path.exists());
        Config::write_default_if_missing(&path).unwrap();
        assert!(path.exists());
        // The bootstrap file is valid JSON (and carries no secret).
        let written = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(value["live_api"]["gemini"]["api_key"], "");
        // It round-trips back to the defaults; a model is supplied via env since none is shipped.
        figment::Jail::expect_with(|jail| {
            jail.set_env("JOI_LIVE_API__GEMINI__MODEL", "m");
            let cfg = Config::load_from(&path, &test_paths()).unwrap();
            assert_eq!(cfg.live_api.provider, ProviderName::Gemini);
            Ok(())
        });
        // A second call must not overwrite or error.
        Config::write_default_if_missing(&path).unwrap();
    }

    #[test]
    fn config_round_trips_through_json() {
        // A non-default config serializes to JSON and loads back identically (minus the redacted
        // key, which Joi never persists). This is the property the runtime settings interface and
        // the bootstrap both rely on.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = Config::default();
        cfg.live_api.gemini.model = "gemini-3.1-flash-live-preview".to_string();
        cfg.live_api.gemini.voice = Some("Charon".to_string());
        cfg.media.audio.frame_ms = 30;
        cfg.ui.terminal.accent = "#ff0066".to_string();
        cfg.write_json(&path).unwrap();

        figment::Jail::expect_with(|_jail| {
            let loaded = Config::load_from(&path, &test_paths()).unwrap();
            assert_eq!(loaded.live_api.gemini.voice.as_deref(), Some("Charon"));
            assert_eq!(loaded.media.audio.frame_ms, 30);
            assert_eq!(loaded.ui.terminal.accent, "#ff0066");
            Ok(())
        });
    }

    #[test]
    fn legacy_yaml_config_is_migrated_to_json() {
        // FR/SETTINGS: a pre-JSON `~/.joi/config` (YAML) is converted to `config.json` the first
        // time `load` runs against the default location, leaving the legacy file as a backup.
        figment::Jail::expect_with(|jail| {
            let dir = jail.directory().to_path_buf();
            let legacy = dir.join("config"); // YAML, extensionless (the old layout)
            let target = dir.join("config.json");
            std::fs::write(
                &legacy,
                "live_api:\n  gemini:\n    model: migrated-model\n    voice: Fenrir\n",
            )
            .unwrap();

            let mut paths = test_paths();
            paths.config_file = target.clone();
            paths.legacy_config_file = legacy.clone();
            assert!(!target.exists());

            // The migration runs inside `load`'s logic; exercise it directly via the helper +
            // load_from (load() resolves real ~/.joi paths, which we can't use under a Jail).
            Config::migrate_legacy_to_json(&legacy, &target).unwrap();
            assert!(target.exists(), "config.json written");
            assert!(legacy.exists(), "legacy file kept as a backup");
            // The migrated file is JSON and carries the legacy values.
            let cfg = Config::load_from(&target, &paths).unwrap();
            assert_eq!(cfg.live_api.gemini.model, "migrated-model");
            assert_eq!(cfg.live_api.gemini.voice.as_deref(), Some("Fenrir"));
            Ok(())
        });
    }

    #[test]
    fn file_overrides_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.json",
                r#"
{
  "live_api": { "provider": "mock", "gemini": { "model": "test-model" } },
  "media": { "audio": { "frame_ms": 40 } }
}
"#,
            )?;
            let cfg = Config::load_from(Path::new("joi.json"), &test_paths()).unwrap();
            assert_eq!(cfg.live_api.provider, ProviderName::Mock);
            assert_eq!(cfg.live_api.gemini.model, "test-model");
            assert_eq!(cfg.media.audio.frame_ms, 40);
            // unspecified nested fields keep their defaults (deep merge)
            assert_eq!(cfg.live_api.gemini.voice.as_deref(), Some("Aoede"));
            assert_eq!(cfg.media.audio.input_device, "default");
            Ok(())
        });
    }

    #[test]
    fn api_key_from_file_and_nested_joi_env() {
        // In the file…
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.json",
                r#"{ "live_api": { "gemini": { "model": "m", "api_key": "file-key" } } }"#,
            )?;
            let cfg = Config::load_from(Path::new("joi.json"), &test_paths()).unwrap();
            assert_eq!(cfg.live_api.gemini.api_key.get(), Some("file-key"));
            Ok(())
        });
        // …and the nested JOI_ env form wins over the file.
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.json",
                r#"{ "live_api": { "gemini": { "model": "m", "api_key": "file-key" } } }"#,
            )?;
            jail.set_env("JOI_LIVE_API__GEMINI__API_KEY", "env-key");
            let cfg = Config::load_from(Path::new("joi.json"), &test_paths()).unwrap();
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
                "joi.json",
                r#"{ "live_api": { "provider": "mock", "gemini": { "model": "from-file" } } }"#,
            )?;
            jail.set_env("JOI_LIVE_API__GEMINI__MODEL", "from-env");
            jail.set_env("JOI_MEDIA__AUDIO__FRAME_MS", "30");
            let cfg = Config::load_from(Path::new("joi.json"), &test_paths()).unwrap();
            assert_eq!(cfg.live_api.gemini.model, "from-env");
            assert_eq!(cfg.media.audio.frame_ms, 30);
            Ok(())
        });
    }

    #[test]
    fn invalid_value_is_rejected() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "joi.json",
                r#"{ "live_api": { "gemini": { "model": "m" } }, "media": { "audio": { "frame_ms": 500 } } }"#,
            )?;
            let err = Config::load_from(Path::new("joi.json"), &test_paths()).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }));
            Ok(())
        });
    }
}
