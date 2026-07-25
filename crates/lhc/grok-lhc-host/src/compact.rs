//! Compact bridge mode: shadow (default) vs replace (opt-in). Chunk 2.
//!
//! Mutually exclusive by construction — a single enum consulted once per
//! decision point, never two independent booleans.

/// Who writes compaction for a session request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMode {
    /// LHC disabled — native compaction only (and no LHC preview).
    Off,
    /// Native drives; LHC `preview_compact` records what it would do.
    Shadow,
    /// LHC `compact` drives; native auto-compact is suppressed.
    Replace,
}

impl CompactMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Replace => "replace",
        }
    }

    /// Native compaction may write the conversation.
    pub fn native_writes(self) -> bool {
        matches!(self, Self::Off | Self::Shadow)
    }

    /// LHC compact may write the conversation.
    pub fn lhc_writes(self) -> bool {
        matches!(self, Self::Replace)
    }

    /// LHC should run preview for comparison / certification.
    pub fn lhc_previews(self) -> bool {
        matches!(self, Self::Shadow)
    }
}

/// Resolve compact mode from the environment. Single source of truth.
///
/// - `GROK_LHC` unset/false → [`CompactMode::Off`]
/// - `GROK_LHC=1` and `GROK_LHC_COMPACT=replace` → [`CompactMode::Replace`]
/// - `GROK_LHC=1` otherwise → [`CompactMode::Shadow`]
pub fn compact_mode() -> CompactMode {
    if !crate::gating::is_enabled() {
        return CompactMode::Off;
    }
    match std::env::var("GROK_LHC_COMPACT") {
        Ok(v) if v.trim().eq_ignore_ascii_case("replace") => CompactMode::Replace,
        _ => CompactMode::Shadow,
    }
}

/// Test helper: pin mode for the current process (certification only).
#[cfg(any(test, feature = "test-util"))]
pub fn set_compact_mode_for_test(mode: Option<CompactMode>) {
    *test_mode_slot().lock().unwrap_or_else(|e| e.into_inner()) = mode;
}

#[cfg(any(test, feature = "test-util"))]
fn test_mode_slot() -> &'static std::sync::Mutex<Option<CompactMode>> {
    use std::sync::OnceLock;
    static SLOT: OnceLock<std::sync::Mutex<Option<CompactMode>>> = OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Resolve mode, honoring test override when present.
pub fn resolve_compact_mode() -> CompactMode {
    #[cfg(any(test, feature = "test-util"))]
    {
        if let Ok(guard) = test_mode_slot().lock()
            && let Some(mode) = *guard
        {
            return mode;
        }
    }
    compact_mode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_mutually_exclusive_writers() {
        assert!(CompactMode::Off.native_writes() && !CompactMode::Off.lhc_writes());
        assert!(CompactMode::Shadow.native_writes() && !CompactMode::Shadow.lhc_writes());
        assert!(!CompactMode::Replace.native_writes() && CompactMode::Replace.lhc_writes());
        // Never both:
        for mode in [CompactMode::Off, CompactMode::Shadow, CompactMode::Replace] {
            assert!(
                !(mode.native_writes() && mode.lhc_writes()),
                "{mode:?} must not allow two writers"
            );
        }
    }

    #[test]
    fn env_replace_opt_in() {
        let _lock = crate::gating::env_lock();
        let prev_lhc = std::env::var_os("GROK_LHC");
        let prev_c = std::env::var_os("GROK_LHC_COMPACT");
        unsafe {
            std::env::set_var("GROK_LHC", "1");
            std::env::set_var("GROK_LHC_COMPACT", "replace");
        }
        assert_eq!(compact_mode(), CompactMode::Replace);
        unsafe {
            std::env::remove_var("GROK_LHC_COMPACT");
        }
        assert_eq!(compact_mode(), CompactMode::Shadow);
        match prev_lhc {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
        match prev_c {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_COMPACT", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_COMPACT") },
        }
    }
}
