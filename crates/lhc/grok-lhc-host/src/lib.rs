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
pub use compact::{CompactMode, compact_mode, resolve_compact_mode};
pub use gating::{is_enabled, lhc_root};
pub use inference::{
    LhcInferenceError, LhcInferenceErrorKind, LhcInferenceFuture, LhcInferenceOp,
    LhcInferenceSample, LhcInferenceSampler, MockLhcInferenceSampler, register_inference_sampler,
    unregister_inference_sampler,
};
/// Re-exported so the shell sampler can stamp provenance without depending on `lhc` directly.
pub use lhc::shared_tech::{InferenceRequestMessage, InferenceRequestRole};
pub use mapping::{MappedEvent, level_label, map_history, map_item, map_model_change};
pub use serving::{
    ServeDecision, apply_serve_decision, body_has_tool_cycle, context_to_conversation_items,
    decide_substitution, split_system_prefix,
};
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
pub use compact::set_compact_mode_for_test;

#[cfg(any(test, feature = "test-util"))]
pub use gating::env_lock;

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

/// Chunk 2 serving entry: fetch LHC context and decide substitution.
///
/// Fail-open: any error yields [`ServeDecision::Native`]. Never hybrid.
/// Safe to call from the shell's async turn loop (no blocking).
pub async fn serve_request_context(
    session_id: &str,
    native_items: Vec<xai_grok_sampling_types::ConversationItem>,
) -> ServeDecision {
    if !is_enabled() || !capture_active(session_id) {
        return ServeDecision::Native {
            reason: "lhc_disabled_or_inactive",
        };
    }
    let Some(handle) = lookup_session(session_id) else {
        return ServeDecision::Native {
            reason: "no_capture_handle",
        };
    };
    match handle.get_llm_request_context().await {
        Ok(ctx) => decide_substitution(&native_items, &ctx),
        Err(err) => {
            tracing::error!(
                session_id,
                %err,
                "LHC serving: get_llm_request_context failed; native path"
            );
            ServeDecision::Native {
                reason: "get_llm_request_context_failed",
            }
        }
    }
}

/// Shadow-mode: preview LHC compaction without writing. Logs and returns.
pub async fn shadow_preview_compact(session_id: &str) {
    if !matches!(resolve_compact_mode(), CompactMode::Shadow) {
        return;
    }
    let Some(handle) = lookup_session(session_id) else {
        return;
    };
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

/// Replace-mode: run LHC compact. Returns whether LHC wrote successfully.
pub async fn replace_compact(session_id: &str) -> bool {
    if !matches!(resolve_compact_mode(), CompactMode::Replace) {
        return false;
    }
    let Some(handle) = lookup_session(session_id) else {
        tracing::error!(session_id, "LHC compact replace: no capture handle");
        return false;
    };
    match handle.compact_thread().await {
        Ok(_receipt) => {
            tracing::info!(session_id, "LHC compact replace: compact complete");
            true
        }
        Err(err) => {
            tracing::error!(
                session_id,
                %err,
                "LHC compact replace: compact failed; native path should resume"
            );
            false
        }
    }
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
}
