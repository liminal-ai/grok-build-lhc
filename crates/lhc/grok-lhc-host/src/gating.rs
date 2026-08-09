//! Feature gate: LHC capture is **on by default** in this fork.
//!
//! Set `GROK_LHC=0` / `false` / `off` (or `[lhc] enabled = false` via config
//! apply) to disable for troubleshooting. Explicit `1` / `true` / `on` forces
//! on. Truthy/falsey sets match [`crate::runtime_config`] so resolve and gate
//! cannot disagree.

/// Return whether LHC capture should be installed for a new session.
///
/// Reads `GROK_LHC` once per call; the host calls this at session spawn and
/// caches the decision by whether a capture handle is registered. When the
/// env var is unset, the fork default is **enabled**.
pub fn is_enabled() -> bool {
    match std::env::var("GROK_LHC") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
        }
        Err(_) => true,
    }
}

/// Root directory for LHC registry + thread files.
///
/// Override with `GROK_LHC_ROOT` (tests). Default: `~/.lhc` (LHC registry
/// convention — see `lhc::threads` registry path).
pub fn lhc_root() -> std::path::PathBuf {
    if let Ok(root) = std::env::var("GROK_LHC_ROOT") {
        let trimmed = root.trim();
        if !trimmed.is_empty() {
            return std::path::PathBuf::from(trimmed);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".lhc")
}

/// Process-wide lock for tests that mutate `GROK_LHC*` env vars.
#[cfg(any(test, feature = "test-util"))]
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_when_unset() {
        let _g = env_lock();
        // SAFETY: test process; GROK_LHC restored below.
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::remove_var("GROK_LHC") };
        assert!(is_enabled(), "fork default is on when GROK_LHC is unset");
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }

    #[test]
    fn disabled_when_explicitly_off() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::set_var("GROK_LHC", "0") };
        assert!(!is_enabled());
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }
}
