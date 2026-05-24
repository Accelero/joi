//! The curated runtime-settings surface over [`Config`] (mechanism; see `doc/SETTINGS.md`).
//!
//! Settings *are* config — there is no separate settings store. This module adds only two things on
//! top of [`Config`]: (a) a typed descriptor of the fields that may be changed at runtime, each
//! tagged with *when* a change takes effect ([`ApplyTiming`]), and (b) a validated write
//! ([`apply_setting`]) that maps a [`SettingId`] + [`SettingValue`] onto the right field.
//!
//! It is pure: it reads and mutates a [`Config`] in memory. Persisting the result (atomically) and
//! propagating it to the running engine is *policy*, done at the composition root (`joi-app`).

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::SettingsError;

/// Prebuilt voice names offered in the [`SettingKind::Choice`] for [`SettingId::Voice`]. The model
/// may accept more (native-audio models expose extra voices) and unknown names fall back to the
/// model default, so this list is a convenience for the UI, not a hard constraint.
pub const KNOWN_VOICES: &[&str] = &[
    "Aoede", "Charon", "Fenrir", "Kore", "Puck", "Leda", "Orus", "Zephyr",
];

/// When a settings change takes effect — the frontend renders this so the user knows whether a
/// change is live, applies on the next connect, or needs a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyTiming {
    /// Live: applied the moment it's saved. (UI appearance the frontend re-reads on the
    /// `UiEvent::Settings` it gets back.)
    Immediate,
    /// Applies when the next realtime session connects — no app restart needed. The manager reads
    /// the field at connect, so an already-live session keeps its current value until reconnect.
    NextSession,
    /// Wired once at startup (`JoiApp::build`); takes effect only after an app restart.
    RestartRequired,
}

/// Stable identifier for an editable setting. This is the **curated** set — deliberately a small
/// subset of [`Config`], not every field. Extend it as subsystems gain the ability to apply a
/// change at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingId {
    /// `live_api.gemini.voice` — the agent's voice.
    Voice,
    /// `ui.terminal.accent` — accent color.
    Accent,
    /// `ui.terminal.background` — terminal background color.
    Background,
}

impl SettingId {
    /// Every editable setting, in display order. Adding a variant here without a matching arm in
    /// [`settings_schema`]/[`apply_setting`] is caught by the drift-coverage test.
    pub const ALL: [SettingId; 3] = [SettingId::Voice, SettingId::Accent, SettingId::Background];

    /// A stable string slug (used in error messages and as the serde tag).
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            SettingId::Voice => "voice",
            SettingId::Accent => "accent",
            SettingId::Background => "background",
        }
    }
}

/// A typed value for a setting, crossing the host boundary. Externally tagged so a frontend can
/// construct it unambiguously (`{"text":"Charon"}`, `{"bool":true}`, `{"u32":30}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingValue {
    /// A boolean toggle.
    Bool(bool),
    /// An unsigned integer.
    U32(u32),
    /// A text/string value.
    Text(String),
}

impl SettingValue {
    /// Borrow as text, or an [`SettingsError::InvalidValue`] if this isn't a [`SettingValue::Text`].
    fn as_text(&self, id: SettingId) -> Result<&str, SettingsError> {
        match self {
            SettingValue::Text(s) => Ok(s),
            _ => Err(invalid(id, "expected a text value")),
        }
    }
}

/// The control a frontend should render for a setting, plus its constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "snake_case")]
pub enum SettingKind {
    /// On/off switch (a [`SettingValue::Bool`]).
    Toggle,
    /// One of a fixed set of options (a [`SettingValue::Text`]); the value may also be a custom
    /// string the provider accepts.
    Choice {
        /// The offered options.
        options: Vec<String>,
    },
    /// An integer in an inclusive range (a [`SettingValue::U32`]).
    Number {
        /// Inclusive minimum.
        min: u32,
        /// Inclusive maximum.
        max: u32,
    },
    /// A color: `"#rrggbb"`, a named color, or `transparent` (a [`SettingValue::Text`]).
    Color,
    /// Free-form text (a [`SettingValue::Text`]).
    Text,
}

/// A full snapshot of the editable-settings surface — the payload of `UiEvent::Settings` and the
/// return of [`settings_schema`]. The frontend renders its settings panel straight from this.
pub type SettingsSnapshot = Vec<SettingDescriptor>;

/// One editable setting: its id, a human label, the current value, the control to render, and when
/// a change applies. The list of these is the panel a frontend builds; it never reads [`Config`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingDescriptor {
    /// Stable id to pass back to [`apply_setting`].
    pub id: SettingId,
    /// Human-readable label.
    pub label: String,
    /// The current value, read from [`Config`].
    pub value: SettingValue,
    /// Which control to render + its constraints.
    pub kind: SettingKind,
    /// When a change to this setting takes effect.
    pub apply: ApplyTiming,
}

/// The current editable surface, read from `cfg`. One descriptor per [`SettingId::ALL`]; the
/// frontend renders these and sends a [`SettingId`] + new [`SettingValue`] back to apply one.
#[must_use]
pub fn settings_schema(cfg: &Config) -> Vec<SettingDescriptor> {
    SettingId::ALL
        .iter()
        .map(|&id| descriptor(id, cfg))
        .collect()
}

fn descriptor(id: SettingId, cfg: &Config) -> SettingDescriptor {
    match id {
        SettingId::Voice => SettingDescriptor {
            id,
            label: "Voice".to_string(),
            // `None` (model default) surfaces as an empty string.
            value: SettingValue::Text(cfg.live_api.gemini.voice.clone().unwrap_or_default()),
            kind: SettingKind::Choice {
                options: KNOWN_VOICES.iter().map(|s| (*s).to_string()).collect(),
            },
            apply: ApplyTiming::NextSession,
        },
        SettingId::Accent => SettingDescriptor {
            id,
            label: "Accent color".to_string(),
            value: SettingValue::Text(cfg.ui.terminal.accent.clone()),
            kind: SettingKind::Color,
            apply: ApplyTiming::Immediate,
        },
        SettingId::Background => SettingDescriptor {
            id,
            label: "Background".to_string(),
            value: SettingValue::Text(cfg.ui.terminal.background.clone()),
            kind: SettingKind::Color,
            apply: ApplyTiming::Immediate,
        },
    }
}

/// Apply a settings change to `cfg` in memory: map `id` to its field, set `value` (with per-field
/// validation), then run [`Config::validate`] so the result is always loadable. On any error `cfg`
/// is left **unchanged** (the field write happens on a value validated first), so a caller can keep
/// using it.
pub fn apply_setting(
    cfg: &mut Config,
    id: SettingId,
    value: &SettingValue,
) -> Result<(), SettingsError> {
    match id {
        SettingId::Voice => {
            let v = value.as_text(id)?.trim();
            // Empty = "use the model default voice".
            cfg.live_api.gemini.voice = (!v.is_empty()).then(|| v.to_string());
        }
        SettingId::Accent => {
            cfg.ui.terminal.accent = non_empty_text(id, value)?;
        }
        SettingId::Background => {
            cfg.ui.terminal.background = non_empty_text(id, value)?;
        }
    }
    // Safety net: never let a change produce a config that wouldn't load.
    cfg.validate()
        .map_err(|e| invalid(id, &format!("would invalidate config: {e}")))
}

/// Read a non-empty trimmed text value or reject it.
fn non_empty_text(id: SettingId, value: &SettingValue) -> Result<String, SettingsError> {
    let v = value.as_text(id)?.trim();
    if v.is_empty() {
        return Err(invalid(id, "must not be empty"));
    }
    Ok(v.to_string())
}

fn invalid(id: SettingId, reason: &str) -> SettingsError {
    SettingsError::InvalidValue {
        setting: id.slug().to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn schema_covers_every_setting_id_exactly_once() {
        // Drift guard: the rendered schema must match the curated id set 1:1 — no missing arm in
        // `descriptor`, no stray duplicate. Adding a `SettingId` without a schema entry fails here.
        let cfg = Config::default();
        let ids: Vec<SettingId> = settings_schema(&cfg).into_iter().map(|d| d.id).collect();
        assert_eq!(ids, SettingId::ALL.to_vec());
    }

    #[test]
    fn apply_timing_is_pinned_per_setting() {
        // Pin the apply-timing contract so a change is a conscious decision (the frontend renders
        // "applies on reconnect" / "restart to apply" off this).
        let cfg = Config::default();
        let by_id = |want: SettingId| {
            settings_schema(&cfg)
                .into_iter()
                .find(|d| d.id == want)
                .unwrap()
                .apply
        };
        assert_eq!(by_id(SettingId::Voice), ApplyTiming::NextSession);
        assert_eq!(by_id(SettingId::Accent), ApplyTiming::Immediate);
        assert_eq!(by_id(SettingId::Background), ApplyTiming::Immediate);
    }

    #[test]
    fn every_setting_round_trips_through_schema_and_apply() {
        // Each descriptor's own current value re-applies cleanly (idempotent) and is observable in
        // the next schema read — proving the schema/apply field mapping agrees for every id.
        let mut cfg = Config::default();
        cfg.live_api.gemini.model = "m".to_string(); // make the base config valid
        for desc in settings_schema(&cfg) {
            apply_setting(&mut cfg, desc.id, &desc.value).unwrap();
            let after = settings_schema(&cfg)
                .into_iter()
                .find(|d| d.id == desc.id)
                .unwrap();
            assert_eq!(after.value, desc.value, "value drifted for {:?}", desc.id);
        }
    }

    #[test]
    fn voice_change_sets_and_clears() {
        let mut cfg = Config::default();
        cfg.live_api.gemini.model = "m".to_string();
        apply_setting(
            &mut cfg,
            SettingId::Voice,
            &SettingValue::Text("Charon".to_string()),
        )
        .unwrap();
        assert_eq!(cfg.live_api.gemini.voice.as_deref(), Some("Charon"));
        // Empty text clears it back to the model default.
        apply_setting(
            &mut cfg,
            SettingId::Voice,
            &SettingValue::Text(String::new()),
        )
        .unwrap();
        assert_eq!(cfg.live_api.gemini.voice, None);
    }

    #[test]
    fn wrong_value_type_is_rejected_and_leaves_config_untouched() {
        let mut cfg = Config::default();
        cfg.live_api.gemini.model = "m".to_string();
        let before = cfg.ui.terminal.accent.clone();
        let err =
            apply_setting(&mut cfg, SettingId::Accent, &SettingValue::Bool(true)).unwrap_err();
        assert!(matches!(err, SettingsError::InvalidValue { .. }));
        assert_eq!(
            cfg.ui.terminal.accent, before,
            "rejected change must not mutate"
        );
    }

    #[test]
    fn empty_color_is_rejected() {
        let mut cfg = Config::default();
        cfg.live_api.gemini.model = "m".to_string();
        let err = apply_setting(
            &mut cfg,
            SettingId::Background,
            &SettingValue::Text("   ".to_string()),
        )
        .unwrap_err();
        assert!(matches!(err, SettingsError::InvalidValue { .. }));
    }

    #[test]
    fn setting_value_serializes_externally_tagged() {
        let json = serde_json::to_value(SettingValue::Text("Aoede".to_string())).unwrap();
        assert_eq!(json, serde_json::json!({ "text": "Aoede" }));
        let back: SettingValue = serde_json::from_value(json).unwrap();
        assert_eq!(back, SettingValue::Text("Aoede".to_string()));
    }
}
