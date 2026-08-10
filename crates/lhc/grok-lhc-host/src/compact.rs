//! Compact bridge mode: Off vs Replace (product path).
//!
//! Mutually exclusive by construction — a single enum consulted once per
//! decision point. When LHC is on, Replace is the only compact mode; the
//! only kill switch is `GROK_LHC=0`.

use std::sync::atomic::{AtomicU64, Ordering};

/// Who writes compaction for a session request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMode {
    /// LHC disabled — native compaction only.
    Off,
    /// LHC `compact` drives; native auto-compact is suppressed.
    Replace,
}

impl CompactMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Replace => "replace",
        }
    }

    /// Native compaction may write the conversation.
    pub fn native_writes(self) -> bool {
        matches!(self, Self::Off)
    }

    /// LHC compact may write the conversation.
    pub fn lhc_writes(self) -> bool {
        matches!(self, Self::Replace)
    }
}

/// Pure plan for one logical compact event (no I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactBridgePlan {
    /// Native only — no LHC call.
    NativeOnly,
    /// Attempt `replace_compact` once at the writer choke point.
    ReplaceOnceAtChoke,
}

impl CompactBridgePlan {
    pub fn from_mode(mode: CompactMode) -> Self {
        match mode {
            CompactMode::Off => Self::NativeOnly,
            CompactMode::Replace => Self::ReplaceOnceAtChoke,
        }
    }

    /// Whether the plan may invoke mutating LHC compact.
    pub fn may_replace(self) -> bool {
        matches!(self, Self::ReplaceOnceAtChoke)
    }
}

/// Sticky per-event bridge state. Execute LHC work at most once; fail-open
/// verdicts stay sticky for the remainder of the event.
#[derive(Debug)]
pub struct CompactEventBridge {
    plan: CompactBridgePlan,
    lhc_attempts: u32,
    /// After a failed replace, never retry LHC for this event.
    fail_open: bool,
    /// LHC successfully wrote; native must not.
    lhc_wrote: bool,
}

/// Outcome after consulting the bridge at the native writer choke point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactChokeAction {
    /// Proceed with native compaction (LHC off or fail-open).
    RunNative,
    /// Skip native — LHC already wrote for this event.
    SkipNativeLhcWrote,
    /// Skip native — still waiting / should not happen after `drive_*`.
    SkipNativePending,
}

impl CompactEventBridge {
    pub fn new(mode: CompactMode) -> Self {
        Self {
            plan: CompactBridgePlan::from_mode(mode),
            lhc_attempts: 0,
            fail_open: false,
            lhc_wrote: false,
        }
    }

    pub fn plan(&self) -> CompactBridgePlan {
        self.plan
    }

    pub fn lhc_attempts(&self) -> u32 {
        self.lhc_attempts
    }

    pub fn fail_open(&self) -> bool {
        self.fail_open
    }

    pub fn lhc_wrote(&self) -> bool {
        self.lhc_wrote
    }

    /// Whether a mutating LHC replace may still be attempted.
    pub fn should_attempt_replace(&self) -> bool {
        self.plan.may_replace() && !self.fail_open && !self.lhc_wrote && self.lhc_attempts == 0
    }

    /// Record one LHC replace attempt. Fail-open is sticky.
    pub fn record_replace_result(&mut self, ok: bool) {
        self.lhc_attempts = self.lhc_attempts.saturating_add(1);
        if ok {
            self.lhc_wrote = true;
        } else {
            self.fail_open = true;
        }
    }

    /// Action for the native writer after LHC side-effects for this event.
    pub fn choke_action(&self) -> CompactChokeAction {
        if self.lhc_wrote {
            CompactChokeAction::SkipNativeLhcWrote
        } else {
            // Off or replace fail-open → native.
            CompactChokeAction::RunNative
        }
    }
}

/// Process-wide counters for certification (test-util / tests only).
static REPLACE_CALLS: AtomicU64 = AtomicU64::new(0);

#[cfg(any(test, feature = "test-util"))]
pub fn reset_compact_call_counters() {
    REPLACE_CALLS.store(0, Ordering::SeqCst);
}

#[cfg(any(test, feature = "test-util"))]
pub fn replace_call_count() -> u64 {
    REPLACE_CALLS.load(Ordering::SeqCst)
}

pub(crate) fn note_replace_call() {
    REPLACE_CALLS.fetch_add(1, Ordering::SeqCst);
}

/// Resolve compact mode. Single source of truth.
///
/// - `GROK_LHC` unset/true → [`CompactMode::Replace`] — LHC compaction is the
///   product; this fork does not ship it disarmed.
/// - `GROK_LHC=0`/`false`/`off` → [`CompactMode::Off`]
pub fn compact_mode() -> CompactMode {
    if !crate::gating::is_enabled() {
        return CompactMode::Off;
    }
    CompactMode::Replace
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
        assert!(!CompactMode::Replace.native_writes() && CompactMode::Replace.lhc_writes());
        for mode in [CompactMode::Off, CompactMode::Replace] {
            assert!(
                !(mode.native_writes() && mode.lhc_writes()),
                "{mode:?} must not allow two writers"
            );
        }
    }

    #[test]
    fn replace_is_the_only_live_mode() {
        let _lock = crate::gating::env_lock();
        let prev_lhc = std::env::var_os("GROK_LHC");
        unsafe {
            std::env::set_var("GROK_LHC", "1");
        }
        assert_eq!(
            compact_mode(),
            CompactMode::Replace,
            "enabled means Replace, no staging gates"
        );
        unsafe {
            std::env::set_var("GROK_LHC", "0");
        }
        assert_eq!(
            compact_mode(),
            CompactMode::Off,
            "the kill switch is the only gate"
        );
        match prev_lhc {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }

    #[test]
    fn bridge_replace_success_is_once() {
        let mut b = CompactEventBridge::new(CompactMode::Replace);
        assert!(b.should_attempt_replace());
        b.record_replace_result(true);
        assert!(!b.should_attempt_replace());
        assert_eq!(b.lhc_attempts(), 1);
        assert_eq!(b.choke_action(), CompactChokeAction::SkipNativeLhcWrote);
    }

    #[test]
    fn bridge_fail_open_is_sticky_and_allows_native() {
        let mut b = CompactEventBridge::new(CompactMode::Replace);
        b.record_replace_result(false);
        assert!(b.fail_open());
        assert!(
            !b.should_attempt_replace(),
            "must not retry after fail-open"
        );
        assert_eq!(b.choke_action(), CompactChokeAction::RunNative);
        assert!(!b.should_attempt_replace());
        assert_eq!(b.lhc_attempts(), 1);
    }

    #[test]
    fn bridge_first_fail_second_succeed_impossible_without_reset() {
        let mut b = CompactEventBridge::new(CompactMode::Replace);
        b.record_replace_result(false);
        assert!(!b.should_attempt_replace());
        b.record_replace_result(true);
        assert_eq!(b.lhc_attempts(), 2);
    }
}
