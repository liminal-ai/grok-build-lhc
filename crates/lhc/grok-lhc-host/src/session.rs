//! Per-session LHC instance / thread lifecycle.
//!
//! Capture identity lives only in LHC (registry + thread SQLite). Generation is
//! latched from `BatchResult.thread_position.last_event_order` — never a sidecar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use lhc::sdk::{Lhc, OpResult, SdkConfig, ThreadRef, init_lhc};
use lhc::shared_tech::SdkMode;
use lhc::shared_tech::errors::ErrorCode;
use lhc::thread_view::CompactAbortSignal;
use lhc::threads::{ListThreadsInput, NewThreadInput, ResolveInput};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::gating::lhc_root;
use crate::idempotency::{OccurrenceTracker, seed_occurrence_from_keys};
use crate::inference::inference_callbacks_for_session;

#[cfg(any(test, feature = "test-util"))]
use std::sync::atomic::{AtomicBool, Ordering};

/// Serialize registry schema init — concurrent `new_thread` races on CREATE TABLE.
fn registry_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

/// Compact abort outcome surfaced in `/lhc status` (not a drain wait state).
///
/// Background mode drains continuously; compact never waits on derivation.
/// The only host-visible compact outcome worth recording is abandon-before-install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactDrainOutcome {
    /// Turn abort / cancel stopped compact before a snapshot install.
    AbandonedByCancel,
}

impl CompactDrainOutcome {
    pub fn status_line(self) -> &'static str {
        match self {
            Self::AbandonedByCancel => {
                "compact abandoned — turn abort (no LHC snapshot install / write-back)"
            }
        }
    }
}

fn compact_outcome_registry() -> &'static Mutex<HashMap<String, CompactDrainOutcome>> {
    static REG: OnceLock<Mutex<HashMap<String, CompactDrainOutcome>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn note_compact_drain_outcome(session_id: &str, outcome: CompactDrainOutcome) {
    if let Ok(mut map) = compact_outcome_registry().lock() {
        map.insert(session_id.to_string(), outcome);
    }
}

/// Last compact abort outcome for `/lhc status`.
pub fn last_compact_drain_outcome(session_id: &str) -> Option<CompactDrainOutcome> {
    compact_outcome_registry()
        .lock()
        .ok()
        .and_then(|map| map.get(session_id).copied())
}

#[cfg(any(test, feature = "test-util"))]
fn sever_compact_signal_flag() -> &'static std::sync::atomic::AtomicBool {
    static FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    &FLAG
}

/// Test-only: when true, compact passes `signal: None` (R1 break-watch).
#[cfg(any(test, feature = "test-util"))]
pub fn set_sever_compact_signal_for_test(sever: bool) {
    sever_compact_signal_flag().store(sever, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(any(test, feature = "test-util"))]
fn sever_compact_signal_for_test() -> bool {
    sever_compact_signal_flag().load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(any(test, feature = "test-util")))]
fn sever_compact_signal_for_test() -> bool {
    false
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
    /// Latched from LHC `last_event_order`. Used as the key-generation
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
    /// events. Refuses to open if `list_events` fails.
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
        let registry_path = root.join("registry.sqlite");
        let file_path = thread_file_path(root, session_id);

        let callbacks = {
            #[cfg(any(test, feature = "test-util"))]
            {
                if use_deterministic_inference_for_test() {
                    lhc::create_deterministic_inference_callbacks()
                } else {
                    inference_callbacks_for_session(session_id)
                }
            }
            #[cfg(not(any(test, feature = "test-util")))]
            {
                inference_callbacks_for_session(session_id)
            }
        };
        // Always Background — pi-lhc / t3code reference shape. Derivation drains
        // via the SDK scheduler after each intake commit; first-touch catch-up
        // absorbs backlog at open. No host poke / idle pump.
        let lhc = init_lhc(SdkConfig {
            inference_callbacks: Some(callbacks),
            inference: None,
            mode: SdkMode::Background,
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
                    "LHC: list_events failed at open; refusing to open the session"
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

    /// Latch tip from a batch result — monotonic `max`.
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
    ///
    /// Compact is a **selection walk with the fallback ladder** — immediately
    /// and unconditionally. It never waits on derivation: missing material
    /// degrades visibly; background drain upgrades stored forms for the next
    /// compact. There is no time budget on this path.
    ///
    /// Pass a [`CancellationToken`] + live [`CompactAbortSignal`] so turn abort
    /// prevents snapshot install. Compact compute after thread resolution
    /// is **synchronous** — `tokio::select!` cannot preempt it; the port signal
    /// (live `Arc<AtomicBool>` re-read at `compact_stopped`) is what stops the
    /// write. Callers that cancel mid-compute **must** also call
    /// [`CompactAbortSignal::abort`] (the production
    /// [`crate::replace_compact_for_writeback`] DropGuard does this on drop —
    /// no OS bridge thread). Prefer that entry point.
    ///
    /// Background derivation continuing after abort is correct — not a leak.
    pub async fn compact(
        &self,
        cancel: CancellationToken,
        signal: CompactAbortSignal,
    ) -> Result<lhc::shared_tech::view::CompactReceipt, String> {
        if cancel.is_cancelled() {
            signal.abort();
            note_compact_drain_outcome(&self.session_id, CompactDrainOutcome::AbandonedByCancel);
            return Err("compact_cancelled".into());
        }

        let params = {
            #[cfg(any(test, feature = "test-util"))]
            {
                take_compact_params_override_for_test()
            }
            #[cfg(not(any(test, feature = "test-util")))]
            {
                None
            }
        };
        let opts = lhc::thread_view::CompactOpts {
            profile: None,
            params,
            // Live CompactAbortSignal — port re-reads `.aborted()` at each
            // checkpoint; do not snapshot a bool here.
            signal: if sever_compact_signal_for_test() {
                None
            } else {
                Some(signal.clone())
            },
        };
        // Race cancel against the async compact future only — not a drain
        // budget. Sync compute inside still depends on CompactAbortSignal.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                signal.abort();
                note_compact_drain_outcome(
                    &self.session_id,
                    CompactDrainOutcome::AbandonedByCancel,
                );
                Err("compact_cancelled".into())
            }
            result = self.lhc.thread_view.compact(self.thread_ref.clone(), opts) => {
                match result {
                    OpResult::Ok { value } => {
                        if cancel.is_cancelled() || signal.aborted() {
                            note_compact_drain_outcome(
                                &self.session_id,
                                CompactDrainOutcome::AbandonedByCancel,
                            );
                            Err("compact_cancelled".into())
                        } else {
                            Ok(value)
                        }
                    }
                    OpResult::Err { error } => {
                        if error.code == ErrorCode::CompactStopped
                            || cancel.is_cancelled()
                            || signal.aborted()
                        {
                            note_compact_drain_outcome(
                                &self.session_id,
                                CompactDrainOutcome::AbandonedByCancel,
                            );
                            Err("compact_cancelled".into())
                        } else {
                            Err(error.reason)
                        }
                    }
                }
            }
        }
    }

    /// View derivation / visibility status (`lhc.thread_view.status`).
    pub async fn get_view_status(&self) -> Result<lhc::shared_tech::view::ViewStatus, String> {
        match self.lhc.thread_view.status(self.thread_ref.clone()).await {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    /// Inspect health (derivation ready/pending counts + queue).
    pub async fn inspect_health(&self) -> Result<lhc::shared_tech::inspect::HealthReport, String> {
        match self.lhc.inspect.health(self.thread_ref.clone()).await {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    /// Await background-scheduler quiescence for this thread.
    ///
    /// Production runs [`SdkMode::Background`]; the scheduler drains after each
    /// intake poke. This waits — it does **not** start a host-driven
    /// `work.drain`. Used by tests / cert to observe settle-between-turns, and
    /// (capped) by [`Self::close`].
    pub async fn drain_settled(&self) {
        self.lhc.drain_settled(self.thread_ref.clone()).await;
    }

    /// Tear down the session: capped [`Lhc::drain_settled`], then drop.
    ///
    /// The sole host-side drain-related call (pi-lhc dispose / t3code stop).
    /// With Background mode, work has been draining continuously — this is a
    /// short settle for in-flight items, not a six-minute catch-up bill.
    pub async fn close(self) {
        match tokio::time::timeout(
            crate::DRAIN_SETTLED_AT_CLOSE,
            self.lhc.drain_settled(self.thread_ref.clone()),
        )
        .await
        {
            Ok(()) => {
                debug!(
                    session_id = %self.session_id,
                    "LHC: close drainSettled completed"
                );
            }
            Err(_) => {
                warn!(
                    session_id = %self.session_id,
                    timeout_secs = crate::DRAIN_SETTLED_AT_CLOSE.as_secs(),
                    "LHC: close drainSettled timed out — proceeding with teardown"
                );
            }
        }
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
    // Require registry binding — refuse orphans.
    match lhc
        .threads
        .resolve(ResolveInput {
            thread_id: info.thread_id.clone(),
            registry_path: Some(registry_str.to_string()),
        })
        .await
    {
        OpResult::Ok { value } => {
            // registry/file disagreement is unsafe (could attach this ACP
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

/// Injective filename encoding for ACP session ids.
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

/// True when registry-resolved path is not the session's expected file.
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

/// Arm/disarm whole-index classify failure.
#[cfg(any(test, feature = "test-util"))]
pub fn set_force_classify_list_failure(armed: bool) {
    FORCE_CLASSIFY_LIST_FAILURE.store(armed, std::sync::atomic::Ordering::SeqCst);
}

/// Test-only: open sessions with [`lhc::create_deterministic_inference_callbacks`]
/// instead of the host sampler bridge (Chunk 3B harness — no network).
#[cfg(any(test, feature = "test-util"))]
static USE_DETERMINISTIC_INFERENCE: AtomicBool = AtomicBool::new(false);

#[cfg(any(test, feature = "test-util"))]
fn use_deterministic_inference_for_test() -> bool {
    USE_DETERMINISTIC_INFERENCE.load(Ordering::SeqCst)
}

#[cfg(any(test, feature = "test-util"))]
pub fn set_use_deterministic_inference_for_test(armed: bool) {
    USE_DETERMINISTIC_INFERENCE.store(armed, Ordering::SeqCst);
}

/// Test-only: one-shot `ViewCompactParams` for the next [`LhcSession::compact`].
///
/// Production always passes `params: None`. The Chunk 3B harness arms tight
/// params so deterministic callbacks can emit typed bands through the host
/// Replace choke (same triple as the N3 probe). Cleared after one compact.
#[cfg(any(test, feature = "test-util"))]
static COMPACT_PARAMS_OVERRIDE: std::sync::Mutex<
    Option<lhc::shared_tech::view::ViewCompactParams>,
> = std::sync::Mutex::new(None);

#[cfg(any(test, feature = "test-util"))]
fn take_compact_params_override_for_test() -> Option<lhc::shared_tech::view::ViewCompactParams> {
    COMPACT_PARAMS_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

#[cfg(any(test, feature = "test-util"))]
pub fn set_compact_params_override_for_test(
    params: Option<lhc::shared_tech::view::ViewCompactParams>,
) {
    *COMPACT_PARAMS_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = params;
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
