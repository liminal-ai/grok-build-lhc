//! `ChatPersistence` tee decorator — the whole Chunk 1 capture hook.
//!
//! The tee keeps a **per-session fast binding**: a [`RegistrySnapshot`]
//! (generation + handle) produced atomically under the registry mutex.
//! Steady-state persists compare one atomic to the cached generation and use
//! the cached value — including cached `None` — without taking the registry
//! mutex. When the generation moves, the next persist re-resolves
//! via [`lookup_session_snapshot`] only — there is no production path that
//! stamps the cache from a bare `registry_generation()` plus a separate
//! `lookup_session`.

use std::sync::Arc;

use tokio::sync::oneshot;
use xai_chat_state::{ChatPersistence, StrictAppendAck, StrictAppendError};
use xai_grok_sampling_types::{ConversationItem, TokenUsage};

use crate::capture::{
    CaptureHandle, RegistrySnapshot, lookup_session_snapshot, registry_generation, spawn_capture,
};
use crate::gating::is_enabled;
use crate::inference::LhcInferenceSampler;
use crate::mapping::token_usage_to_provider_usage;

/// Wrap `inner` with an LHC-resolving tee.
///
/// When `GROK_LHC` is enabled at spawn, also starts the capture worker.
/// When disabled at spawn, still installs the tee so a later `/lhc on`
/// (`spawn_capture`) is observed on subsequent persists — without a
/// seventh runtime hook or mutating `ChatStateActor`'s persistence slot.
///
/// Disabled persist path (steady state, any other session's state): one
/// generation atomic compare; no registry mutex, no SQLite.
pub fn tee_chat_persistence(
    session_id: &str,
    cwd: &str,
    bootstrap: &[ConversationItem],
    inner: Box<dyn ChatPersistence>,
    sampler: Option<Arc<dyn LhcInferenceSampler>>,
) -> Box<dyn ChatPersistence> {
    if is_enabled() && spawn_capture(session_id, Some(cwd), bootstrap, None, sampler).is_none() {
        tracing::warn!(
            session_id,
            "LHC: capture worker failed to start; resolving tee still installed"
        );
    }
    Box::new(LhcTeePersistence {
        inner,
        session_id: session_id.to_string(),
        cached: lookup_session_snapshot(session_id),
    })
}

/// True when a capture worker is registered for `session_id`.
///
/// Post-spawn gating uses registry presence, not a re-read of `GROK_LHC`.
/// Cheap process-wide atomic first — no registry mutex when LHC is off entirely.
pub fn capture_active(session_id: &str) -> bool {
    if !crate::capture::any_capture_active() {
        return false;
    }
    crate::capture::is_session_registered(session_id)
}

struct LhcTeePersistence {
    inner: Box<dyn ChatPersistence>,
    session_id: String,
    /// Last atomic registry snapshot for this session.
    cached: RegistrySnapshot,
}

impl LhcTeePersistence {
    /// Refresh the cached binding from an atomic registry snapshot.
    ///
    /// Production path: only [`lookup_session_snapshot`]. The test-only racy
    /// branch reconstructs the pre-AB1 two-observation call site so the
    /// suite can fail when that assembly is restored.
    fn refresh_binding(&mut self) {
        self.cached = lookup_session_snapshot(&self.session_id);
        // Test-only: interleave after the atomic stamp (mid-session-on race).
        #[cfg(any(test, feature = "test-util"))]
        crate::capture::take_and_run_refresh_interleave_hook();
    }

    /// Resolve the live handle for this session, if any.
    ///
    /// Steady state: generation atomic only. Mutex only when generation moved.
    fn with_handle(&mut self, f: impl FnOnce(&CaptureHandle)) {
        let current = registry_generation();
        if self.cached.generation() != current {
            self.refresh_binding();
        }
        if let Some(handle) = self.cached.handle()
            && !handle.is_closed()
        {
            f(handle);
        }
    }
}

impl ChatPersistence for LhcTeePersistence {
    fn persist_message(&mut self, item: &ConversationItem) {
        self.persist_message_with_provider_usage(item, None);
    }

    fn persist_message_with_provider_usage(
        &mut self,
        item: &ConversationItem,
        provider_usage: Option<&TokenUsage>,
    ) {
        let usage_map = provider_usage.and_then(token_usage_to_provider_usage);
        self.with_handle(|h| h.persist_with_provider_usage(item, usage_map));
        // Inner (disk) path does not need usage — chat_history.jsonl is native shape.
        self.inner.persist_message(item);
    }

    fn persist_working_directory_switch_and_ack(
        &mut self,
        item: &ConversationItem,
    ) -> oneshot::Receiver<Result<StrictAppendAck, StrictAppendError>> {
        self.with_handle(|h| h.persist(item));
        self.inner.persist_working_directory_switch_and_ack(item)
    }

    fn replace_history(&mut self, items: &[ConversationItem]) {
        self.with_handle(|h| h.replace_history(items));
        self.inner.replace_history(items);
    }

    fn flush(&mut self) {
        self.with_handle(|h| h.flush_async());
        self.inner.flush();
    }
}

impl Drop for LhcTeePersistence {
    fn drop(&mut self) {
        // Session teardown: clear last-serve label and shut down any live
        // worker for this session. Fire-and-forget — never block on an
        // async runtime.
        crate::serving::clear_last_serve_outcome(&self.session_id);
        let snap = lookup_session_snapshot(&self.session_id);
        if let Some(handle) = snap.into_handle()
            && !handle.is_closed()
        {
            handle.shutdown_async();
        }
    }
}
