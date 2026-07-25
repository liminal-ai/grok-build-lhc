//! Grok Build host adapter for LHC (fork-only crate; see `/FORK.md`).
//!
//! Chunk 1: packaging, session/thread lifecycle, exhaustive
//! `ConversationItem` → `MessageEventInput` mapping, idempotent capture tee,
//! and model/thinking-level change tee.
//!
//! Chunk 2: ModelCall inference adapter, request-context serving (shell hook),
//! and compact bridge (shadow / replace).
//!
//! Mapping decisions live in `MAPPING.md`.

mod capture;
mod compact;
mod equivalence;
mod gating;
mod idempotency;
mod inference;
mod mapping;
mod serving;
mod session;
mod tee;

pub use capture::{
    CAPTURE_QUEUE_CAP, CaptureHandle, any_capture_active, is_session_registered, lookup_session,
    spawn_capture,
};

#[cfg(any(test, feature = "test-util"))]
pub use capture::{registry_lookup_count, reset_registry_lookup_count};
pub use compact::{
    CompactBridgePlan, CompactChokeAction, CompactEventBridge, CompactMode, compact_mode,
    resolve_compact_mode,
};
pub use equivalence::{
    EquivalenceReport, EquivalenceSnapshot, ProjectedItem, compare_serve_equivalence,
    equivalence_armed, equivalence_snapshot, normalize_whitespace, observe_serve_equivalence,
    project_conversation_canonical,
};
pub use gating::{is_enabled, lhc_root};
pub use inference::{
    CountingLhcInferenceSampler, LhcInferenceError, LhcInferenceErrorKind, LhcInferenceFuture,
    LhcInferenceOp, LhcInferenceRequest, LhcInferenceSample, LhcInferenceSampler,
    MockLhcInferenceSampler, inference_callbacks_for_session, register_inference_sampler,
    unregister_inference_sampler,
};
/// Re-exported so the shell sampler can stamp provenance without depending on `lhc` directly.
pub use lhc::shared_tech::{InferenceRequestMessage, InferenceRequestRole};
pub use mapping::{MappedEvent, level_label, map_history, map_item, map_model_change};
pub use serving::{
    ServeDecision, SourceKindIndex, ViewTranslateMode, apply_serve_decision, assign_prompt_indices,
    assign_prompt_indices_from_tail, body_has_tool_cycle, build_writeback_conversation,
    decide_substitution, is_band_user, native_prompt_indices, positional_user_count,
    session_view_to_items, session_view_to_serve_items, session_view_to_writeback_items,
    split_system_prefix,
};
#[cfg(any(test, feature = "test-util"))]
pub use session::set_force_classify_list_failure;
pub use session::{encode_session_id_for_path, paths_disagree, thread_file_path};
pub use tee::{capture_active, tee_chat_persistence};

/// Test-only: open an LHC session without spawning the capture worker.
#[cfg(feature = "test-util")]
pub async fn session_open_for_test(
    session_id: &str,
    root: &std::path::Path,
) -> Option<session::LhcSession> {
    session::LhcSession::open(session_id, None, Some(root))
        .await
        .map(|(s, _)| s)
}

#[cfg(feature = "test-util")]
pub use session::LhcSession;

#[cfg(any(test, feature = "test-util"))]
pub use compact::{
    preview_call_count, replace_call_count, reset_compact_call_counters, set_compact_mode_for_test,
};

#[cfg(any(test, feature = "test-util"))]
pub use gating::env_lock;

#[cfg(any(test, feature = "test-util"))]
pub use inference::inference_sampler_registered;

#[cfg(any(test, feature = "test-util"))]
pub use equivalence::{
    informational_hit_count, reset_equivalence_counters, serve_compared_turns,
    serve_fallback_turns, structural_hit_count,
};

/// Re-exported so the tripwire compile genuinely links the vendored port.
pub use lhc::sdk::init_lhc;

/// LHC-HOOK target: model / thinking-level change tee.
///
/// No-op when no capture session is registered (the spawn-time gate, F9).
/// Suppresses no-op transitions. Host must pass the authoritative previous
/// model / thinking level (F5).
pub fn capture_model_or_thinking_change(
    session_id: &str,
    previous_model: &str,
    new_model: &str,
    previous_thinking_level: Option<&str>,
    new_thinking_level: Option<&str>,
) {
    // Cheap process-wide gate before any allocation / mutex (A9). Do not
    // re-read GROK_LHC — registry presence is the cached gate (F9).
    if !any_capture_active() {
        return;
    }
    let Some(handle) = lookup_session(session_id) else {
        return;
    };
    let prev_level = level_label(previous_thinking_level);
    let new_level = level_label(new_thinking_level);
    if previous_model == new_model && prev_level == new_level {
        return;
    }
    handle.model_change(previous_model, new_model, &prev_level, &new_level);
}

/// Fire-and-forget session teardown. Safe from async contexts (F4).
pub fn shutdown_session(session_id: &str) {
    if let Some(handle) = lookup_session(session_id) {
        handle.shutdown_async();
    }
}

/// Bound on the serving wait for `get_session_thread_view` (request path).
pub const SERVE_CONTEXT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Chunk 2 serving entry: fetch typed session view and decide substitution.
///
/// Fail-open: any error / timeout yields [`ServeDecision::Native`]. Never hybrid.
/// Cheap gate: registry presence only (no env re-read, no clone of items here).
/// Callers must skip this entirely when [`capture_active`] is false.
pub async fn serve_request_context(
    session_id: &str,
    native_items: &[xai_grok_sampling_types::ConversationItem],
) -> ServeDecision {
    // Cheap process-wide gate before any allocation / mutex — same principle
    // as hook 3 (`capture_model_or_thinking_change`). Do not re-read GROK_LHC.
    if !any_capture_active() || !capture_active(session_id) {
        return ServeDecision::Native {
            reason: "lhc_inactive",
        };
    }
    let Some(handle) = lookup_session(session_id) else {
        return ServeDecision::Native {
            reason: "no_capture_handle",
        };
    };
    // One worker round-trip: session view + messages.list kind index.
    // Not per-entry — cost is one list alongside the view fetch.
    let (view, kinds) =
        match tokio::time::timeout(SERVE_CONTEXT_TIMEOUT, handle.get_classify_context()).await {
            Ok(Ok(ctx)) => ctx,
            Ok(Err(err)) => {
                tracing::error!(
                    session_id,
                    %err,
                    "LHC serving: get_classify_context failed; native path"
                );
                return ServeDecision::Native {
                    reason: "get_classify_context_failed",
                };
            }
            Err(_) => {
                tracing::error!(
                    session_id,
                    timeout_ms = SERVE_CONTEXT_TIMEOUT.as_millis() as u64,
                    "LHC serving: get_classify_context timed out; native path"
                );
                return ServeDecision::Native {
                    reason: "get_classify_context_timeout",
                };
            }
        };
    decide_substitution(native_items, &view, &kinds)
}

/// Shadow-mode: preview LHC compaction without writing. Logs and returns.
pub async fn shadow_preview_compact(session_id: &str) {
    if !matches!(resolve_compact_mode(), CompactMode::Shadow) {
        return;
    }
    let Some(handle) = lookup_session(session_id) else {
        return;
    };
    compact::note_preview_call();
    match handle.preview_compact().await {
        Ok(outcome) => {
            tracing::info!(
                session_id,
                outcome = ?outcome,
                "LHC compact shadow: preview_compact complete"
            );
        }
        Err(err) => {
            tracing::warn!(session_id, %err, "LHC compact shadow: preview_compact failed");
        }
    }
}

/// Result of a successful Replace-mode compact, ready for host write-back.
#[derive(Debug)]
pub struct ReplaceCompactWriteback {
    /// LHC compact receipt total (view tokens) — diagnostics only.
    pub receipt_total_tokens: i64,
    /// Post-compact typed session view (structure + text for write-back).
    pub view: lhc::shared_tech::view::SessionThreadView,
    /// Once-per-translation `message_id` → kind index (RuntimeNote vs prompt).
    pub kinds: SourceKindIndex,
}

/// Replace-mode: run LHC compact and fetch the post-compact session view.
///
/// Unreachable unless [`CompactMode::Replace`] is active (experimental opt-in).
/// On success the host **must** write the body into native state via
/// `replace_conversation_for_compaction` (see MAPPING.md). Flag-off remains
/// the rollback at every point.
pub async fn replace_compact_for_writeback(
    session_id: &str,
) -> Result<ReplaceCompactWriteback, String> {
    if !matches!(resolve_compact_mode(), CompactMode::Replace) {
        return Err("replace_mode_inactive".into());
    }
    let Some(handle) = lookup_session(session_id) else {
        tracing::error!(session_id, "LHC compact replace: no capture handle");
        return Err("no_capture_handle".into());
    };
    compact::note_replace_call();
    let receipt = handle.compact_thread().await.map_err(|err| {
        tracing::error!(
            session_id,
            %err,
            "LHC compact replace: compact failed; native path should resume"
        );
        err
    })?;
    tracing::info!(
        session_id,
        receipt_total = receipt.total_tokens,
        "LHC compact replace: compact complete"
    );
    let (view, kinds) = handle.get_classify_context().await.map_err(|err| {
        tracing::error!(
            session_id,
            %err,
            "LHC compact replace: post-compact classify context fetch failed"
        );
        err
    })?;
    Ok(ReplaceCompactWriteback {
        receipt_total_tokens: receipt.total_tokens,
        view,
        kinds,
    })
}

/// Replace-mode boolean helper (tests / counters). Prefer
/// [`replace_compact_for_writeback`] at the production choke.
pub async fn replace_compact(session_id: &str) -> bool {
    replace_compact_for_writeback(session_id).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_chat_state::NullChatPersistence;
    use xai_grok_sampling_types::ConversationItem;

    #[test]
    fn vendored_lhc_links() {
        let _ = super::init_lhc;
    }

    #[test]
    fn disabled_path_installs_no_decorator() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        let prev_root = std::env::var_os("GROK_LHC_ROOT");
        unsafe {
            std::env::remove_var("GROK_LHC");
            std::env::remove_var("GROK_LHC_ROOT");
        }
        let session_id = "disabled-path-test-session";
        let out =
            tee_chat_persistence(session_id, "/tmp", &[], Box::new(NullChatPersistence), None);
        assert!(
            !capture_active(session_id),
            "disabled path must not install LHC capture"
        );
        let mut p = out;
        p.persist_message(&ConversationItem::user("x"));
        p.flush();
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
        match prev_root {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
        }
    }

    /// L3 — per-turn `capture_active` must not take the registry mutex when
    /// no capture worker is live (cheap atomic only). Lib tests never spawn
    /// capture, so the process-wide gate stays clear here.
    #[test]
    fn disabled_path_capture_active_touches_no_registry() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        unsafe {
            std::env::remove_var("GROK_LHC");
        }
        assert!(
            !any_capture_active(),
            "lib unit tests must not leave capture workers registered"
        );
        reset_registry_lookup_count();
        let before = registry_lookup_count();
        assert!(!capture_active("l3-no-capture-session"));
        assert_eq!(
            registry_lookup_count(),
            before,
            "capture_active must not touch the registry when any_capture_active is false"
        );
        // Simulate a disabled-path turn consulting the gate repeatedly.
        for _ in 0..8 {
            let _ = any_capture_active() && capture_active("l3-no-capture-session");
        }
        assert_eq!(registry_lookup_count(), before);
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }
}
