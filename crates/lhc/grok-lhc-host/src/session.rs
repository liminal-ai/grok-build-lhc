//! Per-session LHC instance / thread lifecycle.
//!
//! Capture identity lives only in LHC (registry + thread SQLite). Generation is
//! latched from `BatchResult.thread_position.last_event_order` — never a sidecar.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use lhc::sdk::{Lhc, OpResult, SdkConfig, ThreadRef, init_lhc};
use lhc::shared_tech::SdkMode;
use lhc::threads::{ListThreadsInput, NewThreadInput, ResolveInput};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, info, warn};

use crate::gating::lhc_root;
use crate::idempotency::{OccurrenceTracker, seed_occurrence_from_keys};
use crate::inference::inference_callbacks_for_session;

/// Serialize registry schema init — concurrent `new_thread` races on CREATE TABLE.
fn registry_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

/// Live LHC capture session (owns the SDK instance + thread path).
pub struct LhcSession {
    pub session_id: String,
    pub lhc: Lhc,
    pub thread_ref: ThreadRef,
    #[allow(dead_code)]
    pub file_path: PathBuf,
    #[allow(dead_code)]
    pub registry_path: PathBuf,
    /// Latched from LHC `last_event_order` (A1). Used as the key-generation
    /// coordinate for subsequent `persist_message` events.
    pub generation: u64,
    /// Next ordinal for model/thinking change keys; seeded from list_events.
    pub next_change_ordinal: u64,
    /// After persistent failures, further capture is disabled for this session.
    pub capture_disabled: bool,
    failure_count: u32,
}

impl LhcSession {
    /// Create or reopen the per-session thread under `root` (or `GROK_LHC_ROOT` / `~/.lhc`).
    ///
    /// Returns the session and an occurrence tracker seeded from LHC's stored
    /// events (B1). Refuses to open if `list_events` fails (B2).
    pub async fn open(
        session_id: &str,
        cwd: Option<&str>,
        root: Option<&Path>,
    ) -> Option<(Self, OccurrenceTracker)> {
        let root_buf = root.map(|p| p.to_path_buf()).unwrap_or_else(lhc_root);
        let root = root_buf.as_path();
        if let Err(err) = std::fs::create_dir_all(root.join("threads")) {
            error!(?err, "LHC: failed to create threads directory");
            return None;
        }
        // Best-effort: remove legacy sidecars from the round-1 scheme (A1).
        let legacy_meta = thread_meta_path(root, session_id);
        if legacy_meta.exists() {
            let _ = std::fs::remove_file(&legacy_meta);
        }

        let registry_path = root.join("registry.sqlite");
        let file_path = thread_file_path(root, session_id);

        let lhc = init_lhc(SdkConfig {
            inference_callbacks: Some(inference_callbacks_for_session(session_id)),
            inference: None,
            mode: SdkMode::Manual,
            clock: None,
            guards: None,
            tool_result: None,
            lease: None,
            chunk_policy: None,
            view: None,
        });

        let registry_str = registry_path.to_string_lossy().into_owned();
        let thread_ref = if file_path.exists() {
            open_existing(&lhc, session_id, &file_path, &registry_str).await?
        } else {
            create_new(&lhc, session_id, cwd, &file_path, &registry_str).await?
        };

        let mut session = Self {
            session_id: session_id.to_string(),
            lhc,
            thread_ref,
            file_path,
            registry_path,
            generation: 0,
            next_change_ordinal: 0,
            capture_disabled: false,
            failure_count: 0,
        };

        let tracker = match session.seed_from_db().await {
            Ok(t) => t,
            Err(err) => {
                error!(
                    session_id,
                    %err,
                    "LHC: list_events failed at open; refusing (B2)"
                );
                return None;
            }
        };

        Some((session, tracker))
    }

    /// Seed tip, change ordinals, and occurrence tracker from stored events.
    ///
    /// Failure is fatal at open — a silent `generation = 0` would mass-duplicate.
    pub async fn seed_from_db(&mut self) -> Result<OccurrenceTracker, String> {
        let events = self.list_events().await?;
        self.generation = events
            .iter()
            .map(|e| e.event_order())
            .max()
            .unwrap_or(0)
            .max(0) as u64;
        let keys: Vec<&str> = events.iter().map(|e| e.idempotency_key()).collect();
        self.next_change_ordinal =
            crate::idempotency::max_change_ordinal_from_keys(keys.iter().copied());
        Ok(seed_occurrence_from_keys(keys))
    }

    /// Latch tip from a batch result — monotonic `max` (B6).
    pub fn latch_generation_from_batch(&mut self, batch: &lhc::intake_stream::BatchResult) {
        let tip = batch.thread_position.last_event_order.max(0) as u64;
        self.generation = self.generation.max(tip);
    }

    /// Submit a batch. Returns the full batch result for outcome inspection.
    pub async fn submit_events(
        &mut self,
        events: &[lhc::intake_stream::MessageEventInput],
    ) -> Result<lhc::intake_stream::BatchResult, String> {
        if self.capture_disabled {
            return Err("capture disabled for session".into());
        }
        if events.is_empty() {
            return Ok(lhc::intake_stream::BatchResult {
                events: vec![],
                turn_transitions: vec![],
                queued_work: vec![],
                thread_position: lhc::intake_stream::ThreadPosition {
                    last_event_order: self.generation as i64,
                },
            });
        }
        let result = self
            .lhc
            .intake_stream
            .message_events(self.thread_ref.clone(), events)
            .await;
        match result {
            OpResult::Ok { value } => {
                self.failure_count = 0;
                self.latch_generation_from_batch(&value);
                Ok(value)
            }
            OpResult::Err { error } => {
                self.failure_count = self.failure_count.saturating_add(1);
                let msg = format!(
                    "LHC message_events failed (class={:?} code={:?}): {}",
                    error.error_class, error.code, error.reason
                );
                error!(session_id = %self.session_id, %msg);
                if self.failure_count >= 3 {
                    warn!(
                        session_id = %self.session_id,
                        "LHC: disabling further capture after repeated failures"
                    );
                    self.capture_disabled = true;
                }
                Err(msg)
            }
        }
    }

    pub async fn list_events(&self) -> Result<Vec<lhc::intake_stream::EventRecord>, String> {
        match self
            .lhc
            .intake_stream
            .list_events(self.thread_ref.clone())
            .await
        {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    /// LHC assembled request context (model wire shape; not used for classification).
    pub async fn get_llm_request_context(
        &self,
    ) -> Result<lhc::shared_tech::view::LlmRequestContext, String> {
        match self
            .lhc
            .thread_view
            .get_llm_request_context(self.thread_ref.clone())
            .await
        {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    /// Typed session thread view — structure + text for serve/write-back.
    pub async fn get_session_thread_view(
        &self,
    ) -> Result<lhc::shared_tech::view::SessionThreadView, String> {
        match self
            .lhc
            .thread_view
            .get_session_thread_view(self.thread_ref.clone())
            .await
        {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    /// View + once-per-translation `message_id` → [`MessageKind`] index.
    ///
    /// One `messages.list` alongside the view (not per entry). **Whole-index
    /// failure propagates as `Err`** — serving falls open to Native; write-back
    /// must not proceed. Per-entry unknown ids still fail toward synthetic
    /// inside the translator (different granularity).
    pub async fn get_classify_context(
        &self,
    ) -> Result<
        (
            lhc::shared_tech::view::SessionThreadView,
            crate::serving::SourceKindIndex,
        ),
        String,
    > {
        #[cfg(any(test, feature = "test-util"))]
        if force_classify_list_failure() {
            return Err("messages_list_failed: forced_test_failure".into());
        }
        let view = self.get_session_thread_view().await?;
        let kinds = match self.lhc.messages.list(self.thread_ref.clone(), None).await {
            OpResult::Ok { value } => crate::serving::SourceKindIndex::from_message_records(&value),
            OpResult::Err { error } => {
                warn!(
                    session_id = %self.session_id,
                    reason = %error.reason,
                    "LHC: messages.list failed; aborting classify context"
                );
                return Err(format!("messages_list_failed: {}", error.reason));
            }
        };
        Ok((view, kinds))
    }

    /// Preview what LHC compaction would do (shadow mode).
    pub async fn preview_compact(
        &self,
    ) -> Result<lhc::shared_tech::view::PreviewCompactOutcome, String> {
        let opts = lhc::thread_view::CompactOpts {
            profile: None,
            params: None,
            signal: None,
        };
        match self
            .lhc
            .thread_view
            .preview_compact(self.thread_ref.clone(), opts)
            .await
        {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    /// Apply LHC compaction (replace mode).
    pub async fn compact(&self) -> Result<lhc::shared_tech::view::CompactReceipt, String> {
        let opts = lhc::thread_view::CompactOpts {
            profile: None,
            params: None,
            signal: None,
        };
        match self
            .lhc
            .thread_view
            .compact(self.thread_ref.clone(), opts)
            .await
        {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    /// View derivation / visibility status (`lhc.thread_view.status`).
    pub async fn get_view_status(&self) -> Result<lhc::shared_tech::view::ViewStatus, String> {
        match self.lhc.thread_view.status(self.thread_ref.clone()).await {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    pub async fn close(self) {
        self.lhc.drain_settled(self.thread_ref.clone()).await;
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn poison(&mut self) {
        self.capture_disabled = false;
        self.failure_count = 2;
        self.thread_ref = ThreadRef::file_path("/nonexistent/lhc-poisoned.sqlite");
    }

    #[allow(dead_code)]
    pub fn resolve_input_for_tests(&self) -> ResolveInput {
        ResolveInput {
            thread_id: String::new(),
            registry_path: Some(self.registry_path.to_string_lossy().into_owned()),
        }
    }
}

async fn open_existing(
    lhc: &Lhc,
    session_id: &str,
    file_path: &Path,
    registry_str: &str,
) -> Option<ThreadRef> {
    let path_str = file_path.to_string_lossy().into_owned();
    // Confirm the file is a real LHC thread (identity lives in the DB).
    let info = match lhc
        .threads
        .info(ThreadRef::file_path(path_str.clone()))
        .await
    {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => {
            error!(
                session_id,
                path = %file_path.display(),
                reason = %error.reason,
                "LHC: thread file exists but info() failed; refusing"
            );
            return None;
        }
    };
    // Require registry binding (F8) — refuse orphans.
    match lhc
        .threads
        .resolve(ResolveInput {
            thread_id: info.thread_id.clone(),
            registry_path: Some(registry_str.to_string()),
        })
        .await
    {
        OpResult::Ok { value } => {
            // B5: registry/file disagreement is unsafe (could attach this ACP
            // session to another session's transcript). Refuse rather than adopt.
            if value.file_path != path_str && Path::new(&value.file_path) != file_path {
                error!(
                    session_id,
                    expected = %file_path.display(),
                    resolved = %value.file_path,
                    "LHC: registry file_path disagrees with session layout; refusing"
                );
                return None;
            }
            let resolved = ThreadRef::file_path(value.file_path);
            match lhc.threads.resolve_thread_ref(resolved.clone()).await {
                OpResult::Ok { .. } => {
                    info!(
                        session_id,
                        thread_id = %info.thread_id,
                        "LHC: reopened thread via registry resolve"
                    );
                    Some(resolved)
                }
                OpResult::Err { error } => {
                    error!(
                        session_id,
                        reason = %error.reason,
                        "LHC: resolve_thread_ref failed for existing thread"
                    );
                    None
                }
            }
        }
        OpResult::Err { error } => {
            // Also try list_threads match by file path (prefix-id resolve failed).
            if let OpResult::Ok { value: rows } = lhc
                .threads
                .list_threads(Some(ListThreadsInput {
                    cwd: None,
                    registry_path: Some(registry_str.to_string()),
                }))
                .await
                && let Some(row) = rows
                    .iter()
                    .find(|r| r.file_path == path_str || Path::new(&r.file_path) == file_path)
            {
                info!(
                    session_id,
                    thread_id = %row.thread_id,
                    "LHC: reopened thread via list_threads file_path match"
                );
                return Some(ThreadRef::file_path(row.file_path.clone()));
            }
            error!(
                session_id,
                path = %file_path.display(),
                thread_id = %info.thread_id,
                reason = %error.reason,
                "LHC: thread file exists without registry binding; refusing"
            );
            None
        }
    }
}

async fn create_new(
    lhc: &Lhc,
    session_id: &str,
    cwd: Option<&str>,
    file_path: &Path,
    registry_str: &str,
) -> Option<ThreadRef> {
    let _guard = registry_lock().lock().await;
    let result = lhc
        .threads
        .new_thread(NewThreadInput {
            file_path: file_path.to_string_lossy().into_owned(),
            title: Some(format!("grok:{session_id}")),
            cwd: cwd.map(|c| c.to_string()),
            registry_path: Some(registry_str.to_string()),
        })
        .await;
    match result {
        OpResult::Ok { value } => {
            info!(
                session_id,
                thread_id = %value.thread_id,
                path = %value.file_path,
                "LHC: created thread"
            );
            let id_ref = ThreadRef::Id(lhc::threads::ThreadRefId {
                thread_id: value.thread_id,
                registry_path: Some(registry_str.to_string()),
            });
            match lhc.threads.resolve_thread_ref(id_ref).await {
                OpResult::Ok { value: resolved } => Some(ThreadRef::file_path(resolved.file_path)),
                OpResult::Err { error } => {
                    error!(
                        session_id,
                        reason = %error.reason,
                        "LHC: resolve after new_thread failed"
                    );
                    None
                }
            }
        }
        OpResult::Err { error } => {
            error!(
                session_id,
                code = ?error.code,
                reason = %error.reason,
                "LHC: new_thread failed"
            );
            None
        }
    }
}

pub fn thread_file_path(root: &Path, session_id: &str) -> PathBuf {
    root.join("threads").join(format!(
        "grok-{}.sqlite",
        encode_session_id_for_path(session_id)
    ))
}

fn thread_meta_path(root: &Path, session_id: &str) -> PathBuf {
    root.join("threads").join(format!(
        "grok-{}.meta.json",
        encode_session_id_for_path(session_id)
    ))
}

/// Injective filename encoding for ACP session ids (B4).
///
/// Percent-encodes every byte outside `[A-Za-z0-9_-]` so `a:b` and `a_b` cannot
/// collide on one thread file.
pub fn encode_session_id_for_path(session_id: &str) -> String {
    let mut out = String::with_capacity(session_id.len());
    for &b in session_id.as_bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('%');
            out.push(nibble_hex(b >> 4));
            out.push(nibble_hex(b & 0x0f));
        }
    }
    out
}

fn nibble_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + (n - 10)) as char,
        _ => '0',
    }
}

/// True when registry-resolved path is not the session's expected file (B5).
pub fn paths_disagree(expected: &Path, registry_file_path: &str) -> bool {
    registry_file_path != expected.to_string_lossy().as_ref()
        && Path::new(registry_file_path) != expected
}

/// Test-only: force `get_classify_context` to fail as if `messages.list` errored.
#[cfg(any(test, feature = "test-util"))]
static FORCE_CLASSIFY_LIST_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-util"))]
fn force_classify_list_failure() -> bool {
    FORCE_CLASSIFY_LIST_FAILURE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Arm/disarm whole-index classify failure (Q1).
#[cfg(any(test, feature = "test-util"))]
pub fn set_force_classify_list_failure(armed: bool) {
    FORCE_CLASSIFY_LIST_FAILURE.store(armed, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_encoding_is_injective_for_sanitized_chars() {
        let a = encode_session_id_for_path("a:b");
        let b = encode_session_id_for_path("a_b");
        assert_ne!(a, b);
        assert!(a.contains("%3A"), "colon must be percent-encoded: {a}");
        assert_eq!(b, "a_b");
    }

    #[test]
    fn path_policy_refuse_on_disagreement() {
        let expected = Path::new("/tmp/threads/grok-s.sqlite");
        let registry = "/tmp/threads/grok-OTHER.sqlite";
        assert!(paths_disagree(expected, registry));
        assert!(!paths_disagree(expected, "/tmp/threads/grok-s.sqlite"));
    }
}
