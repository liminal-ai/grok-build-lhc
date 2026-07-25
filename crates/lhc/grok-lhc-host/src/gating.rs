//! Feature gate: LHC capture is off unless `GROK_LHC` is `1` or `true`.

/// Return whether LHC capture should be installed for a new session.
///
/// Reads `GROK_LHC` once per call; the host calls this at session spawn and
/// caches the decision by whether a capture handle is registered.
pub fn is_enabled() -> bool {
    match std::env::var("GROK_LHC") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
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
    fn disabled_when_unset() {
        let _g = env_lock();
        // SAFETY: test process; GROK_LHC restored below.
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::remove_var("GROK_LHC") };
        assert!(!is_enabled());
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }
}
