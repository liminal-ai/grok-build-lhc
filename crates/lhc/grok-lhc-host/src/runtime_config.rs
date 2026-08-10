//! Process-wide LHC runtime config: resolve, provenance, apply-to-env.
//!
//! **Precedence (highest wins per key):**
//! 1. Environment variable, if set (including explicit `0` / `false` / `off`)
//! 2. Config-file / host-supplied [`LhcFileConfig`] values
//! 3. Built-in defaults (`enabled = true`, root `~/.grok-lhc`,
//!    equivalence armed, no inference-model override)
//!
//! Compact mode is not a config key: when enabled, Replace is always on.
//! Applying a resolved config only **fills unset** env vars so existing tests
//! and gates that set `GROK_LHC*` keep working unchanged. It never overwrites
//! an env var that is already present.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::gating;

/// Values sourced from a host config file (`[lhc]`), before env overlay.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LhcFileConfig {
    pub enabled: Option<bool>,
    pub root: Option<PathBuf>,
    pub equivalence: Option<bool>,
    pub inference_model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Env,
    ConfigFile,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: ConfigSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLhcConfig {
    pub enabled: Sourced<bool>,
    pub root: Sourced<PathBuf>,
    pub equivalence: Sourced<bool>,
    pub inference_model: Sourced<Option<String>>,
}

static APPLIED: OnceLock<ResolvedLhcConfig> = OnceLock::new();

fn env_truthy(name: &str) -> Option<bool> {
    match std::env::var(name) {
        Ok(v) => {
            let t = v.trim();
            if t.is_empty() {
                return None;
            }
            if t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("on") {
                Some(true)
            } else if t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off") {
                Some(false)
            } else {
                // Non-empty unknown → treat as set-but-not-enabling for enable
                // flags; callers that need raw strings read env directly.
                Some(false)
            }
        }
        Err(_) => None,
    }
}

fn env_string(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => {
            let t = v.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Err(_) => None,
    }
}

/// Resolve effective config. Does not mutate the environment.
pub fn resolve_lhc_config(file: &LhcFileConfig) -> ResolvedLhcConfig {
    let enabled = match env_truthy("GROK_LHC") {
        Some(v) => Sourced {
            value: v,
            source: ConfigSource::Env,
        },
        None => match file.enabled {
            Some(v) => Sourced {
                value: v,
                source: ConfigSource::ConfigFile,
            },
            None => Sourced {
                value: true,
                source: ConfigSource::Default,
            },
        },
    };

    let root = match env_string("GROK_LHC_ROOT") {
        Some(v) => Sourced {
            value: PathBuf::from(v),
            source: ConfigSource::Env,
        },
        None => match &file.root {
            Some(v) => Sourced {
                value: v.clone(),
                source: ConfigSource::ConfigFile,
            },
            None => Sourced {
                value: gating::lhc_root(),
                source: ConfigSource::Default,
            },
        },
    };

    // Equivalence: env `0`/`false`/`off` disarms; otherwise default armed.
    // Config file can disarm when env is unset.
    let equivalence = match std::env::var("GROK_LHC_EQUIVALENCE") {
        Ok(v) => {
            let t = v.trim();
            let armed = !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("off"));
            Sourced {
                value: armed,
                source: ConfigSource::Env,
            }
        }
        Err(_) => match file.equivalence {
            Some(v) => Sourced {
                value: v,
                source: ConfigSource::ConfigFile,
            },
            None => Sourced {
                value: true,
                source: ConfigSource::Default,
            },
        },
    };

    let inference_model = match env_string("GROK_LHC_INFERENCE_MODEL") {
        Some(v) => Sourced {
            value: Some(v),
            source: ConfigSource::Env,
        },
        None => match &file.inference_model {
            Some(v) => Sourced {
                value: Some(v.clone()),
                source: ConfigSource::ConfigFile,
            },
            None => Sourced {
                value: None,
                source: ConfigSource::Default,
            },
        },
    };

    ResolvedLhcConfig {
        enabled,
        root,
        equivalence,
        inference_model,
    }
}

/// Apply resolved values into the process environment for keys that are
/// currently unset **and were not sourced from built-in defaults**.
///
/// Default-sourced values must not leak into child processes (Bash / MCP /
/// subagents) and must not make status claim `(source: env)` for a variable
/// the user never set. Gates that need a live env (`is_enabled`,
/// `lhc_root`, compact mode) already fall back to the same defaults when the
/// variable is absent.
///
/// Stores the resolved snapshot for status/provenance display (first apply wins
/// for the snapshot; subsequent applies still fill unset non-default env keys).
pub fn apply_resolved_config(resolved: &ResolvedLhcConfig) {
    let _ = APPLIED.set(resolved.clone());

    // SAFETY: process-level config apply at host startup / re-resolve; tests
    // that mutate GROK_LHC* must hold `env_lock`.
    unsafe {
        // Fill GROK_LHC only for non-default sources so default-on does not
        // leak into child env, while config-file disable still reaches
        // `is_enabled()` (which treats unset as on).
        if std::env::var_os("GROK_LHC").is_none()
            && resolved.enabled.source != ConfigSource::Default
        {
            if resolved.enabled.value {
                std::env::set_var("GROK_LHC", "1");
            } else {
                std::env::set_var("GROK_LHC", "0");
            }
        }
        if std::env::var_os("GROK_LHC_ROOT").is_none()
            && resolved.root.source != ConfigSource::Default
        {
            std::env::set_var("GROK_LHC_ROOT", &resolved.root.value);
        }
        if std::env::var_os("GROK_LHC_EQUIVALENCE").is_none()
            && !resolved.equivalence.value
            && resolved.equivalence.source != ConfigSource::Default
        {
            std::env::set_var("GROK_LHC_EQUIVALENCE", "0");
        }
        if std::env::var_os("GROK_LHC_INFERENCE_MODEL").is_none()
            && resolved.inference_model.source != ConfigSource::Default
            && let Some(ref m) = resolved.inference_model.value
        {
            std::env::set_var("GROK_LHC_INFERENCE_MODEL", m);
        }
    }
}

static CONFIG_PARSE_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Record a malformed `[lhc]` parse failure for status / diagnostics.
pub fn note_config_parse_error(message: impl Into<String>) {
    if let Ok(mut g) = CONFIG_PARSE_ERROR.lock() {
        *g = Some(message.into());
    }
}

/// Clear a previously recorded `[lhc]` parse error (successful re-parse).
pub fn clear_config_parse_error() {
    if let Ok(mut g) = CONFIG_PARSE_ERROR.lock() {
        *g = None;
    }
}

/// Last `[lhc]` parse error, if any.
pub fn config_parse_error() -> Option<String> {
    CONFIG_PARSE_ERROR.lock().ok().and_then(|g| g.clone())
}

/// Last applied resolved config (for `/lhc` provenance), if any.
pub fn applied_config() -> Option<&'static ResolvedLhcConfig> {
    APPLIED.get()
}

impl ConfigSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigSource::Env => "env",
            ConfigSource::ConfigFile => "config",
            ConfigSource::Default => "default",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gating::env_lock;

    #[test]
    fn env_wins_over_file_for_enabled() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::set_var("GROK_LHC", "0") };
        let file = LhcFileConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let r = resolve_lhc_config(&file);
        assert!(!r.enabled.value);
        assert_eq!(r.enabled.source, ConfigSource::Env);
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }

    #[test]
    fn file_enables_when_env_unset() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::remove_var("GROK_LHC") };
        let file = LhcFileConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let r = resolve_lhc_config(&file);
        assert!(r.enabled.value);
        assert_eq!(r.enabled.source, ConfigSource::ConfigFile);
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }

    #[test]
    fn default_is_on() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::remove_var("GROK_LHC") };
        let r = resolve_lhc_config(&LhcFileConfig::default());
        assert!(r.enabled.value);
        assert_eq!(r.enabled.source, ConfigSource::Default);
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }

    #[test]
    fn apply_defaults_do_not_mutate_env() {
        let _g = env_lock();
        let prev_lhc = std::env::var_os("GROK_LHC");
        let prev_root = std::env::var_os("GROK_LHC_ROOT");
        unsafe {
            std::env::remove_var("GROK_LHC");
            std::env::remove_var("GROK_LHC_ROOT");
        }
        let r = resolve_lhc_config(&LhcFileConfig::default());
        assert!(r.enabled.value, "default-on");
        apply_resolved_config(&r);
        assert!(
            std::env::var_os("GROK_LHC").is_none(),
            "default-on must not leak GROK_LHC into process env"
        );
        assert!(
            std::env::var_os("GROK_LHC_ROOT").is_none(),
            "default root must not leak into process env"
        );
        match prev_lhc {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
        match prev_root {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
        }
    }

    #[test]
    fn apply_config_disable_sets_env_zero() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::remove_var("GROK_LHC") };
        let file = LhcFileConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let r = resolve_lhc_config(&file);
        assert!(!r.enabled.value);
        assert_eq!(r.enabled.source, ConfigSource::ConfigFile);
        apply_resolved_config(&r);
        assert_eq!(std::env::var("GROK_LHC").ok().as_deref(), Some("0"));
        assert!(!crate::gating::is_enabled());
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }
}
