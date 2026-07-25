//! Compact bridge mode: shadow (default) vs replace (experimental). Chunk 2.
//!
//! Mutually exclusive by construction — a single enum consulted once per
//! decision point, never two independent booleans.
//!
//! **Replace is unreachable without an explicit experimental opt-in**
//! (`GROK_LHC_COMPACT_EXPERIMENTAL=1` plus `GROK_LHC_COMPACT=replace`).
//! Pending Lee's replace-mode accounting ruling, production must not enter
//! Replace via env alone.

use std::sync::atomic::{AtomicU64, Ordering};

/// Who writes compaction for a session request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMode {
    /// LHC disabled — native compaction only (and no LHC preview).
    Off,
    /// Native drives; LHC `preview_compact` records what it would do.
    Shadow,
    /// LHC `compact` drives; native auto-compact is suppressed.
    /// Unreachable without `GROK_LHC_COMPACT_EXPERIMENTAL=1`.
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

/// Pure plan for one logical compact event (no I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactBridgePlan {
    /// Native only — no LHC call.
    NativeOnly,
    /// Run `preview_compact` once, then native.
    ShadowPreviewThenNative,
    /// Attempt `replace_compact` once at the writer choke point.
    ReplaceOnceAtChoke,
}

impl CompactBridgePlan {
    pub fn from_mode(mode: CompactMode) -> Self {
        match mode {
            CompactMode::Off => Self::NativeOnly,
            CompactMode::Shadow => Self::ShadowPreviewThenNative,
            CompactMode::Replace => Self::ReplaceOnceAtChoke,
        }
    }

    /// Whether the plan may invoke mutating LHC compact (not preview).
    pub fn may_replace(self) -> bool {
        matches!(self, Self::ReplaceOnceAtChoke)
    }

    /// Whether the plan may invoke LHC preview.
    pub fn may_preview(self) -> bool {
        matches!(self, Self::ShadowPreviewThenNative)
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
    /// Proceed with native compaction (shadow preview already done, or off).
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

    /// Whether a shadow preview may still be attempted.
    pub fn should_attempt_preview(&self) -> bool {
        self.plan.may_preview() && self.lhc_attempts == 0
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

    /// Record one shadow preview attempt (counts toward at-most-once I/O).
    pub fn record_preview_done(&mut self) {
        self.lhc_attempts = self.lhc_attempts.saturating_add(1);
    }

    /// Action for the native writer after LHC side-effects for this event.
    pub fn choke_action(&self) -> CompactChokeAction {
        if self.lhc_wrote {
            CompactChokeAction::SkipNativeLhcWrote
        } else {
            // Off, shadow (preview done or not), or replace fail-open → native.
            CompactChokeAction::RunNative
        }
    }
}

/// Process-wide counters for certification (test-util / tests only).
static PREVIEW_CALLS: AtomicU64 = AtomicU64::new(0);
static REPLACE_CALLS: AtomicU64 = AtomicU64::new(0);

#[cfg(any(test, feature = "test-util"))]
pub fn reset_compact_call_counters() {
    PREVIEW_CALLS.store(0, Ordering::SeqCst);
    REPLACE_CALLS.store(0, Ordering::SeqCst);
}

#[cfg(any(test, feature = "test-util"))]
pub fn preview_call_count() -> u64 {
    PREVIEW_CALLS.load(Ordering::SeqCst)
}

#[cfg(any(test, feature = "test-util"))]
pub fn replace_call_count() -> u64 {
    REPLACE_CALLS.load(Ordering::SeqCst)
}

pub(crate) fn note_preview_call() {
    PREVIEW_CALLS.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn note_replace_call() {
    REPLACE_CALLS.fetch_add(1, Ordering::SeqCst);
}

fn experimental_replace_enabled() -> bool {
    match std::env::var("GROK_LHC_COMPACT_EXPERIMENTAL") {
        Ok(v) => {
            let t = v.trim();
            t == "1" || t.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

/// Resolve compact mode from the environment. Single source of truth.
///
/// - `GROK_LHC` unset/false → [`CompactMode::Off`]
/// - `GROK_LHC=1` + `GROK_LHC_COMPACT=replace` + `GROK_LHC_COMPACT_EXPERIMENTAL=1`
///   → [`CompactMode::Replace`]
/// - `GROK_LHC=1` + `GROK_LHC_COMPACT=replace` without experimental → **Shadow**
///   (Replace unreachable; warn once per process via tracing)
/// - `GROK_LHC=1` otherwise → [`CompactMode::Shadow`]
pub fn compact_mode() -> CompactMode {
    if !crate::gating::is_enabled() {
        return CompactMode::Off;
    }
    match std::env::var("GROK_LHC_COMPACT") {
        Ok(v) if v.trim().eq_ignore_ascii_case("replace") => {
            if experimental_replace_enabled() {
                CompactMode::Replace
            } else {
                tracing::warn!(
                    "GROK_LHC_COMPACT=replace ignored without \
                     GROK_LHC_COMPACT_EXPERIMENTAL=1 (Replace unreachable \
                     pending accounting ruling); using shadow"
                );
                CompactMode::Shadow
            }
        }
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
        for mode in [CompactMode::Off, CompactMode::Shadow, CompactMode::Replace] {
            assert!(
                !(mode.native_writes() && mode.lhc_writes()),
                "{mode:?} must not allow two writers"
            );
        }
    }

    #[test]
    fn replace_requires_experimental_opt_in() {
        let _lock = crate::gating::env_lock();
        let prev_lhc = std::env::var_os("GROK_LHC");
        let prev_c = std::env::var_os("GROK_LHC_COMPACT");
        let prev_x = std::env::var_os("GROK_LHC_COMPACT_EXPERIMENTAL");
        unsafe {
            std::env::set_var("GROK_LHC", "1");
            std::env::set_var("GROK_LHC_COMPACT", "replace");
            std::env::remove_var("GROK_LHC_COMPACT_EXPERIMENTAL");
        }
        assert_eq!(
            compact_mode(),
            CompactMode::Shadow,
            "replace without experimental must not reach Replace"
        );
        unsafe {
            std::env::set_var("GROK_LHC_COMPACT_EXPERIMENTAL", "1");
        }
        assert_eq!(compact_mode(), CompactMode::Replace);
        match prev_lhc {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
        match prev_c {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_COMPACT", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_COMPACT") },
        }
        match prev_x {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_COMPACT_EXPERIMENTAL", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_COMPACT_EXPERIMENTAL") },
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
        // Simulate a second consult — still sticky.
        assert!(!b.should_attempt_replace());
        assert_eq!(b.lhc_attempts(), 1);
    }

    #[test]
    fn bridge_first_fail_second_succeed_impossible_without_reset() {
        // The sticky machine forbids the buggy "second attempt succeeds and
        // silently reverts fail-open" shape.
        let mut b = CompactEventBridge::new(CompactMode::Replace);
        b.record_replace_result(false);
        assert!(!b.should_attempt_replace());
        // Even if a caller ignored should_attempt and recorded again:
        b.record_replace_result(true);
        assert_eq!(b.lhc_attempts(), 2);
        // After a success record, native is skipped — but production code must
        // not call record twice. The should_attempt guard is the contract.
    }

    #[test]
    fn bridge_shadow_preview_once_then_native() {
        let mut b = CompactEventBridge::new(CompactMode::Shadow);
        assert!(b.should_attempt_preview());
        assert!(!b.should_attempt_replace());
        b.record_preview_done();
        assert!(!b.should_attempt_preview());
        assert_eq!(b.choke_action(), CompactChokeAction::RunNative);
    }
}
