//! `ChatPersistence` tee decorator — the whole Chunk 1 capture hook.

use std::sync::Arc;

use tokio::sync::oneshot;
use tracing::warn;
use xai_chat_state::{ChatPersistence, StrictAppendAck, StrictAppendError};
use xai_grok_sampling_types::ConversationItem;

use crate::capture::{CaptureHandle, is_session_registered, spawn_capture};
use crate::gating::is_enabled;
use crate::inference::LhcInferenceSampler;

/// Wrap `inner` with LHC capture when `GROK_LHC` is enabled; otherwise return
/// `inner` unchanged (no decorator, no LHC instance, no SQLite, no worker).
///
/// `bootstrap` is the in-memory conversation already loaded from
/// `chat_history.jsonl` (not rewritten). Occurrence indices are seeded from
/// LHC stored events at open; bootstrap is submitted for LHC dedup (B1).
///
/// `sampler` is the Chunk 2 inference transport (dedicated non-main model).
/// Pass `None` only in tests that do not exercise derivation; production
/// shell always supplies a real [`LhcInferenceSampler`].
pub fn tee_chat_persistence(
    session_id: &str,
    cwd: &str,
    bootstrap: &[ConversationItem],
    inner: Box<dyn ChatPersistence>,
    sampler: Option<Arc<dyn LhcInferenceSampler>>,
) -> Box<dyn ChatPersistence> {
    if !is_enabled() {
        return inner;
    }
    let Some(handle) = spawn_capture(session_id, Some(cwd), bootstrap, None, sampler) else {
        tracing::warn!(
            session_id,
            "LHC: capture worker failed to start; host continues"
        );
        return inner;
    };
    Box::new(LhcTeePersistence {
        inner,
        handle,
        capture_stopped: false,
    })
}

/// True when a capture worker is registered for `session_id`.
///
/// Post-spawn gating uses registry presence, not a re-read of `GROK_LHC` (F9).
pub fn capture_active(session_id: &str) -> bool {
    is_session_registered(session_id)
}

struct LhcTeePersistence {
    inner: Box<dyn ChatPersistence>,
    handle: CaptureHandle,
    /// Set once capture is gone (refused open / crash / shutdown). Skips further
    /// clone+try_send and logs the transition only once (D3).
    capture_stopped: bool,
}

impl LhcTeePersistence {
    /// Tee into LHC only while the worker channel is live. Non-blocking.
    fn tee_alive(&mut self) -> bool {
        if self.capture_stopped {
            return false;
        }
        if self.handle.is_closed() {
            warn!(
                session_id = %self.handle.session_id(),
                "LHC: capture channel closed; continuing without tee"
            );
            self.capture_stopped = true;
            return false;
        }
        true
    }
}

impl ChatPersistence for LhcTeePersistence {
    fn persist_message(&mut self, item: &ConversationItem) {
        if self.tee_alive() {
            self.handle.persist(item);
        }
        self.inner.persist_message(item);
    }

    fn persist_working_directory_switch_and_ack(
        &mut self,
        item: &ConversationItem,
    ) -> oneshot::Receiver<Result<StrictAppendAck, StrictAppendError>> {
        if self.tee_alive() {
            self.handle.persist(item);
        }
        self.inner.persist_working_directory_switch_and_ack(item)
    }

    fn replace_history(&mut self, items: &[ConversationItem]) {
        if self.tee_alive() {
            self.handle.replace_history(items);
        }
        self.inner.replace_history(items);
    }

    fn flush(&mut self) {
        if self.tee_alive() {
            self.handle.flush_async();
        }
        self.inner.flush();
    }
}

impl Drop for LhcTeePersistence {
    fn drop(&mut self) {
        // Session teardown signal: actor dropped its ChatPersistence (F4).
        // Fire-and-forget — never block / never blocking_recv on async runtime.
        if !self.capture_stopped && !self.handle.is_closed() {
            self.handle.shutdown_async();
        }
    }
}
