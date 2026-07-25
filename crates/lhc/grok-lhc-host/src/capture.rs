//! Background capture worker + process-wide session registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
#[cfg(any(test, feature = "test-util"))]
use std::thread::JoinHandle;

use lhc::intake_stream::BatchEventOutcome;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, warn};
use xai_grok_sampling_types::ConversationItem;

use crate::idempotency::{ITEM_KEY_GENERATION, OccurrenceTracker};
use crate::inference::{
    LhcInferenceSampler, register_inference_sampler, unregister_inference_sampler,
};
use crate::mapping::{MappedEvent, map_history, map_item, map_model_change};
use crate::session::LhcSession;

/// Bound on the capture queue (F11 / A6). Must not block the chat-state actor.
pub const CAPTURE_QUEUE_CAP: usize = 1024;

/// Process-wide count of live capture workers — cheap gate for model-change (A9).
static ACTIVE_CAPTURES: AtomicUsize = AtomicUsize::new(0);

enum CaptureCmd {
    Persist(ConversationItem),
    ReplaceHistory(Vec<ConversationItem>),
    ModelChange {
        previous_model: String,
        new_model: String,
        previous_level: String,
        new_level: String,
    },
    Flush(oneshot::Sender<()>),
    #[cfg_attr(not(any(test, feature = "test-util")), allow(dead_code))]
    ListEvents(oneshot::Sender<Result<Vec<lhc::intake_stream::EventRecord>, String>>),
    GetLlmRequestContext(
        oneshot::Sender<Result<lhc::shared_tech::view::LlmRequestContext, String>>,
    ),
    PreviewCompact(oneshot::Sender<Result<lhc::shared_tech::view::PreviewCompactOutcome, String>>),
    Compact(oneshot::Sender<Result<lhc::shared_tech::view::CompactReceipt, String>>),
    #[cfg(any(test, feature = "test-util"))]
    Poison(oneshot::Sender<()>),
    #[cfg(any(test, feature = "test-util"))]
    CaptureDisabled(oneshot::Sender<bool>),
    #[cfg(any(test, feature = "test-util"))]
    SubmitRaw {
        events: Vec<lhc::intake_stream::MessageEventInput>,
        ack: oneshot::Sender<Result<lhc::intake_stream::BatchResult, String>>,
    },
    /// Block the worker until `release` fires or crash is signaled (B3).
    #[cfg(any(test, feature = "test-util"))]
    Block {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    /// Fire-and-forget shutdown (ack is optional; Drop never waits).
    Shutdown(Option<oneshot::Sender<()>>),
}

struct CaptureShared {
    session_id: String,
    worker_id: u64,
    tx: mpsc::Sender<CaptureCmd>,
    /// Always-writable shutdown signal (F4) — never blocked by a full queue.
    shutdown_tx: watch::Sender<bool>,
    /// Crash signal: worker exits immediately without drain/close (B3).
    /// Test-only — must not exist in the shipping default-feature build (D4).
    #[cfg(any(test, feature = "test-util"))]
    crash_tx: watch::Sender<bool>,
    /// Shared with the worker (worker must NOT hold `CaptureShared` / the Sender).
    dropped: Arc<AtomicU64>,
    /// Set when a `ReplaceHistory` was dropped — baseline is stale until a
    /// successful replace realigns the tracker (A6).
    baseline_poisoned: Arc<AtomicBool>,
    /// Test-only join handle slot.
    #[cfg(any(test, feature = "test-util"))]
    join: Mutex<Option<JoinHandle<()>>>,
}

/// Handle to a per-session capture worker (cheaply cloneable).
#[derive(Clone)]
pub struct CaptureHandle {
    inner: Arc<CaptureShared>,
}

impl CaptureHandle {
    pub fn persist(&self, item: &ConversationItem) {
        match self.inner.tx.try_send(CaptureCmd::Persist(item.clone())) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.note_drop("persist");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.note_drop("persist_closed");
            }
        }
    }

    pub fn replace_history(&self, items: &[ConversationItem]) {
        match self
            .inner
            .tx
            .try_send(CaptureCmd::ReplaceHistory(items.to_vec()))
        {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.inner.baseline_poisoned.store(true, Ordering::SeqCst);
                let n = self.note_drop("replace_history");
                error!(
                    session_id = %self.inner.session_id,
                    dropped = n,
                    "LHC: dropped replace_history — occurrence baseline poisoned until next successful replace"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.inner.baseline_poisoned.store(true, Ordering::SeqCst);
                self.note_drop("replace_history_closed");
            }
        }
    }

    pub fn model_change(
        &self,
        previous_model: &str,
        new_model: &str,
        previous_level: &str,
        new_level: &str,
    ) {
        match self.inner.tx.try_send(CaptureCmd::ModelChange {
            previous_model: previous_model.to_string(),
            new_model: new_model.to_string(),
            previous_level: previous_level.to_string(),
            new_level: new_level.to_string(),
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.note_drop("model_change");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.note_drop("model_change_closed");
            }
        }
    }

    fn note_drop(&self, kind: &str) -> u64 {
        let n = self.inner.dropped.fetch_add(1, Ordering::Relaxed) + 1; // Arc<AtomicU64>
        let depth = self.queue_depth();
        warn!(
            session_id = %self.inner.session_id,
            dropped = n,
            queue_depth = depth,
            queue_cap = CAPTURE_QUEUE_CAP,
            kind,
            "LHC: capture queue drop"
        );
        n
    }

    /// Fire-and-forget flush (safe on a tokio runtime).
    pub fn flush_async(&self) {
        let (tx, _rx) = oneshot::channel();
        let _ = self.inner.tx.try_send(CaptureCmd::Flush(tx));
    }

    /// Fetch LHC request context from the capture worker (async-safe).
    pub async fn get_llm_request_context(
        &self,
    ) -> Result<lhc::shared_tech::view::LlmRequestContext, String> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .send(CaptureCmd::GetLlmRequestContext(tx))
            .await
            .map_err(|_| "capture worker gone".to_string())?;
        rx.await.map_err(|_| "capture worker dropped".to_string())?
    }

    /// Shadow-mode preview (does not write).
    pub async fn preview_compact(
        &self,
    ) -> Result<lhc::shared_tech::view::PreviewCompactOutcome, String> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .send(CaptureCmd::PreviewCompact(tx))
            .await
            .map_err(|_| "capture worker gone".to_string())?;
        rx.await.map_err(|_| "capture worker dropped".to_string())?
    }

    /// Replace-mode compact (writes).
    pub async fn compact_thread(&self) -> Result<lhc::shared_tech::view::CompactReceipt, String> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .send(CaptureCmd::Compact(tx))
            .await
            .map_err(|_| "capture worker gone".to_string())?;
        rx.await.map_err(|_| "capture worker dropped".to_string())?
    }

    /// Fire-and-forget shutdown — drains then closes (safe from async Drop).
    pub fn shutdown_async(&self) {
        let _ = self.inner.shutdown_tx.send(true);
        let _ = self.inner.tx.try_send(CaptureCmd::Shutdown(None));
    }

    /// Watch-only shutdown signal (no `Shutdown` cmd) — for deterministic A3 tests.
    #[cfg(any(test, feature = "test-util"))]
    pub fn shutdown_watch_only(&self) {
        let _ = self.inner.shutdown_tx.send(true);
    }

    /// Queue-only shutdown cmd (no watch) — for deterministic A3 tests.
    #[cfg(any(test, feature = "test-util"))]
    pub fn shutdown_cmd_only(&self) {
        let _ = self.inner.tx.try_send(CaptureCmd::Shutdown(None));
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    /// True when the worker receiver is gone (refused open, crash, or exit).
    pub fn is_closed(&self) -> bool {
        self.inner.tx.is_closed()
    }

    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    } // via Arc

    /// Approximate queued command depth (cap − remaining capacity).
    pub fn queue_depth(&self) -> usize {
        CAPTURE_QUEUE_CAP.saturating_sub(self.inner.tx.capacity())
    }

    pub fn baseline_poisoned(&self) -> bool {
        self.inner.baseline_poisoned.load(Ordering::SeqCst)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn flush_blocking(&self) {
        let (tx, rx) = oneshot::channel();
        if self.inner.tx.blocking_send(CaptureCmd::Flush(tx)).is_ok() {
            let _ = rx.blocking_recv();
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn shutdown_blocking(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .inner
            .tx
            .blocking_send(CaptureCmd::Shutdown(Some(tx)))
            .is_ok()
        {
            let _ = rx.blocking_recv();
        }
        if let Ok(mut guard) = self.inner.join.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
        unregister_worker(&self.inner.session_id, self.inner.worker_id);
    }

    /// Genuine crash: signal the worker to die without drain/close, drop the
    /// sender (worker does not hold it), detach the join handle (B3).
    #[cfg(any(test, feature = "test-util"))]
    pub fn crash_kill(self) {
        let _ = self.inner.crash_tx.send(true);
        unregister_worker(&self.inner.session_id, self.inner.worker_id);
        if let Ok(mut guard) = self.inner.join.lock() {
            let _ = guard.take(); // detach
        }
        // Dropping self drops the last Sender — channel closes. Worker holds
        // only the Receiver + crash watch, so it cannot drain after death.
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn list_events_blocking(&self) -> Result<Vec<lhc::intake_stream::EventRecord>, String> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .blocking_send(CaptureCmd::ListEvents(tx))
            .map_err(|_| "capture worker gone".to_string())?;
        rx.blocking_recv()
            .map_err(|_| "capture worker dropped".to_string())?
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn poison_blocking(&self) {
        let (tx, rx) = oneshot::channel();
        if self.inner.tx.blocking_send(CaptureCmd::Poison(tx)).is_ok() {
            let _ = rx.blocking_recv();
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn capture_disabled_blocking(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .inner
            .tx
            .blocking_send(CaptureCmd::CaptureDisabled(tx))
            .is_ok()
        {
            rx.blocking_recv().unwrap_or(false)
        } else {
            false
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn submit_raw_blocking(
        &self,
        events: Vec<lhc::intake_stream::MessageEventInput>,
    ) -> Result<lhc::intake_stream::BatchResult, String> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .blocking_send(CaptureCmd::SubmitRaw { events, ack: tx })
            .map_err(|_| "capture worker gone".to_string())?;
        rx.blocking_recv()
            .map_err(|_| "capture worker dropped".to_string())?
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn block_worker(&self, release: oneshot::Receiver<()>) -> oneshot::Receiver<()> {
        let (entered_tx, entered_rx) = oneshot::channel();
        let _ = self.inner.tx.blocking_send(CaptureCmd::Block {
            entered: entered_tx,
            release,
        });
        entered_rx
    }
}

struct RegistryEntry {
    weak: Weak<CaptureShared>,
    worker_id: u64,
}

fn registry() -> &'static Mutex<HashMap<String, RegistryEntry>> {
    static REG: OnceLock<Mutex<HashMap<String, RegistryEntry>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_worker_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn try_register_session(session_id: &str, shared: &Arc<CaptureShared>) -> bool {
    let Ok(mut map) = registry().lock() else {
        return false;
    };
    if let Some(entry) = map.get(session_id)
        && entry.weak.upgrade().is_some()
    {
        return false;
    }
    map.insert(
        session_id.to_string(),
        RegistryEntry {
            weak: Arc::downgrade(shared),
            worker_id: shared.worker_id,
        },
    );
    ACTIVE_CAPTURES.fetch_add(1, Ordering::Relaxed);
    true
}

fn unregister_worker(session_id: &str, worker_id: u64) {
    let Ok(mut map) = registry().lock() else {
        return;
    };
    if matches!(
        map.get(session_id),
        Some(entry) if entry.worker_id == worker_id
    ) {
        map.remove(session_id);
        ACTIVE_CAPTURES.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn lookup_session(session_id: &str) -> Option<CaptureHandle> {
    let map = registry().lock().ok()?;
    let entry = map.get(session_id)?;
    let inner = entry.weak.upgrade()?;
    Some(CaptureHandle { inner })
}

pub fn is_session_registered(session_id: &str) -> bool {
    lookup_session(session_id).is_some()
}

pub fn any_capture_active() -> bool {
    ACTIVE_CAPTURES.load(Ordering::Relaxed) > 0
}

/// Spawn a dedicated OS thread + tokio runtime owning the LHC session.
pub fn spawn_capture(
    session_id: &str,
    cwd: Option<&str>,
    bootstrap: &[ConversationItem],
    root: Option<&Path>,
    sampler: Option<Arc<dyn LhcInferenceSampler>>,
) -> Option<CaptureHandle> {
    let session_id_owned = session_id.to_string();
    let cwd_owned = cwd.map(|c| c.to_string());
    let bootstrap = bootstrap.to_vec();
    let root_owned: Option<PathBuf> = root.map(|p| p.to_path_buf());
    if let Some(sampler) = sampler {
        register_inference_sampler(session_id, sampler);
    }
    let (tx, mut rx) = mpsc::channel::<CaptureCmd>(CAPTURE_QUEUE_CAP);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    #[cfg(any(test, feature = "test-util"))]
    let (crash_tx, mut crash_rx) = watch::channel(false);
    let worker_id = next_worker_id();
    let baseline_poisoned = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));

    let shared = Arc::new(CaptureShared {
        session_id: session_id.to_string(),
        worker_id,
        tx: tx.clone(),
        shutdown_tx,
        #[cfg(any(test, feature = "test-util"))]
        crash_tx,
        dropped: Arc::clone(&dropped),
        baseline_poisoned: Arc::clone(&baseline_poisoned),
        #[cfg(any(test, feature = "test-util"))]
        join: Mutex::new(None),
    });

    if !try_register_session(session_id, &shared) {
        warn!(
            session_id,
            "LHC: capture already registered for session; refusing duplicate spawn"
        );
        return lookup_session(session_id);
    }

    let session_id_for_worker = session_id_owned.clone();
    let worker_id_for_exit = worker_id;
    let baseline_for_worker = Arc::clone(&baseline_poisoned);
    let dropped_for_worker = Arc::clone(&dropped);
    let join = std::thread::Builder::new()
        .name(format!("lhc-capture-{session_id_owned}"))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    error!(?err, "LHC: failed to build capture runtime");
                    unregister_inference_sampler(&session_id_for_worker);
                    unregister_worker(&session_id_for_worker, worker_id_for_exit);
                    return;
                }
            };
            rt.block_on(async move {
                // Open refusal must not block the host caller (C1 / F4). Return
                // the handle immediately; on refuse the worker unregisters and
                // exits so `capture_active` becomes false without a rendezvous.
                let Some((session, mut tracker)) = LhcSession::open(
                    &session_id_for_worker,
                    cwd_owned.as_deref(),
                    root_owned.as_deref(),
                )
                .await
                else {
                    unregister_inference_sampler(&session_id_for_worker);
                    unregister_worker(&session_id_for_worker, worker_id_for_exit);
                    return;
                };

                let mut session = Some(session);
                // Tip is tracked on the session; item keys always use ITEM_KEY_GENERATION.

                // B1: tracker already seeded from LHC events. Bootstrap submit
                // uses a local map (stable keys) then merge_monotonic.
                if bootstrap.is_empty() {
                    if session.as_ref().unwrap().generation > 0 {
                        warn!(
                            session_id = %session_id_for_worker,
                            tip = session.as_ref().unwrap().generation,
                            "LHC: empty bootstrap with existing thread events"
                        );
                    }
                } else {
                    let (events, local) =
                        map_history(&session_id_for_worker, ITEM_KEY_GENERATION, &bootstrap);
                    tracker.merge_monotonic(&local);
                    let inputs: Vec<_> = events.into_iter().map(|e| e.input).collect();
                    match session.as_mut().unwrap().submit_events(&inputs).await {
                        Ok(batch) => {
                            let recorded = batch
                                .events
                                .iter()
                                .filter(|e| matches!(e.outcome, BatchEventOutcome::Recorded))
                                .count();
                            let skipped = batch.events.len() - recorded;
                            debug!(
                                session_id = %session_id_for_worker,
                                recorded,
                                skipped,
                                "LHC: bootstrap submit complete (dedup is the diff)"
                            );
                        }
                        Err(err) => warn!(%err, "LHC: bootstrap submit failed"),
                    }
                }

                loop {
                    // Crash arm exists only under test-util; shipping build awaits
                    // pending forever so select! stays branch-uniform (D4).
                    let crash = async {
                        #[cfg(any(test, feature = "test-util"))]
                        {
                            let _ = crash_rx.changed().await;
                            *crash_rx.borrow()
                        }
                        #[cfg(not(any(test, feature = "test-util")))]
                        {
                            std::future::pending::<()>().await;
                            false
                        }
                    };
                    tokio::select! {
                        cmd = rx.recv() => {
                            let Some(cmd) = cmd else { break; };
                            if let CaptureCmd::Shutdown(ack) = cmd {
                                while let Ok(more) = rx.try_recv() {
                                    #[cfg(any(test, feature = "test-util"))]
                                    if crash_rx.has_changed().is_ok() && *crash_rx.borrow() {
                                        break;
                                    }
                                    let died = process_cmd(
                                        more,
                                        &mut session,
                                        &mut tracker,
                                        &session_id_for_worker,
                                        &baseline_for_worker,
                                        dropped_for_worker.as_ref(),
                                        #[cfg(any(test, feature = "test-util"))]
                                        &mut crash_rx,
                                    )
                                    .await;
                                    if died {
                                        let _ = session.take();
                                        break;
                                    }
                                }
                                if let Some(s) = session.take() {
                                    s.close().await;
                                }
                                if let Some(ack) = ack {
                                    let _ = ack.send(());
                                }
                                break;
                            }
                            if process_cmd(
                                cmd,
                                &mut session,
                                &mut tracker,
                                &session_id_for_worker,
                                &baseline_for_worker,
                                dropped_for_worker.as_ref(),
                                #[cfg(any(test, feature = "test-util"))]
                                &mut crash_rx,
                            )
                            .await
                            {
                                // Crash requested from inside Block.
                                let _ = session.take();
                                break;
                            }
                        }
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                while let Ok(cmd) = rx.try_recv() {
                                    if let CaptureCmd::Shutdown(ack) = cmd {
                                        if let Some(ack) = ack {
                                            let _ = ack.send(());
                                        }
                                        continue;
                                    }
                                    let died = process_cmd(
                                        cmd,
                                        &mut session,
                                        &mut tracker,
                                        &session_id_for_worker,
                                        &baseline_for_worker,
                                        dropped_for_worker.as_ref(),
                                        #[cfg(any(test, feature = "test-util"))]
                                        &mut crash_rx,
                                    )
                                    .await;
                                    if died {
                                        let _ = session.take();
                                        unregister_worker(
                                            &session_id_for_worker,
                                            worker_id_for_exit,
                                        );
                                        return;
                                    }
                                }
                                if let Some(s) = session.take() {
                                    s.close().await;
                                }
                                break;
                            }
                        }
                        crashed = crash => {
                            if crashed {
                                // B3: die immediately — no drain, no close.
                                let _ = session.take();
                                break;
                            }
                        }
                    }
                }

                let _ = session.take();
                unregister_inference_sampler(&session_id_for_worker);
                unregister_worker(&session_id_for_worker, worker_id_for_exit);
            });
        });
    let join = match join {
        Ok(j) => j,
        Err(err) => {
            error!(?err, "LHC: failed to spawn capture thread");
            unregister_worker(session_id, worker_id);
            return None;
        }
    };

    #[cfg(any(test, feature = "test-util"))]
    {
        *shared.join.lock().unwrap_or_else(|e| e.into_inner()) = Some(join);
    }
    #[cfg(not(any(test, feature = "test-util")))]
    {
        let _ = join;
    }

    drop(tx);
    Some(CaptureHandle { inner: shared })
}

/// Process one command. Returns true if the worker should exit (crash).
async fn process_cmd(
    cmd: CaptureCmd,
    session: &mut Option<LhcSession>,
    tracker: &mut OccurrenceTracker,
    session_id: &str,
    baseline_poisoned: &AtomicBool,
    dropped: &AtomicU64,
    #[cfg(any(test, feature = "test-util"))] crash_rx: &mut watch::Receiver<bool>,
) -> bool {
    let Some(sess) = session.as_mut() else {
        return true;
    };
    match cmd {
        CaptureCmd::Persist(item) => {
            if baseline_poisoned.load(Ordering::SeqCst) {
                let n = dropped.fetch_add(1, Ordering::Relaxed) + 1;
                error!(
                    session_id,
                    dropped = n,
                    "LHC: refusing persist while replace_history baseline is poisoned"
                );
                return false;
            }
            // Occurrence advances before submit (B6) — a failed submit leaves a
            // gap so a retry cannot collide with a partially-applied key.
            let mapped = map_item(session_id, ITEM_KEY_GENERATION, &item, tracker);
            let _ = submit_mapped(sess, mapped).await;
            false
        }
        CaptureCmd::ReplaceHistory(items) => {
            // B1: submit the slice; LHC dedup is the diff. Local map mints the
            // same keys survivors already have; merge_monotonic keeps rewind
            // occurrence high-water marks.
            let (events, local) = map_history(session_id, ITEM_KEY_GENERATION, &items);
            tracker.merge_monotonic(&local);
            let _ = submit_mapped(sess, events).await;
            baseline_poisoned.store(false, Ordering::SeqCst);
            debug!(
                session_id,
                baseline_items = items.len(),
                "LHC: replace_history submitted (dedup is the diff)"
            );
            false
        }
        CaptureCmd::ModelChange {
            previous_model,
            new_model,
            previous_level,
            new_level,
        } => {
            if previous_model == new_model && previous_level == new_level {
                return false;
            }
            let ordinal = sess.next_change_ordinal;
            let (mapped, next) = map_model_change(
                session_id,
                &previous_model,
                &new_model,
                &previous_level,
                &new_level,
                ordinal,
            );
            if mapped.is_empty() {
                return false;
            }
            if let Ok(batch) = submit_mapped(sess, mapped).await {
                let any_recorded = batch
                    .events
                    .iter()
                    .any(|e| matches!(e.outcome, BatchEventOutcome::Recorded));
                if any_recorded {
                    sess.next_change_ordinal = next;
                }
            }
            false
        }
        CaptureCmd::Flush(ack) => {
            let _ = ack.send(());
            false
        }
        CaptureCmd::ListEvents(ack) => {
            let _ = ack.send(sess.list_events().await);
            false
        }
        CaptureCmd::GetLlmRequestContext(ack) => {
            let _ = ack.send(sess.get_llm_request_context().await);
            false
        }
        CaptureCmd::PreviewCompact(ack) => {
            let _ = ack.send(sess.preview_compact().await);
            false
        }
        CaptureCmd::Compact(ack) => {
            let _ = ack.send(sess.compact().await);
            false
        }
        #[cfg(any(test, feature = "test-util"))]
        CaptureCmd::Poison(ack) => {
            sess.poison();
            let _ = ack.send(());
            false
        }
        #[cfg(any(test, feature = "test-util"))]
        CaptureCmd::CaptureDisabled(ack) => {
            let _ = ack.send(sess.capture_disabled);
            false
        }
        #[cfg(any(test, feature = "test-util"))]
        CaptureCmd::SubmitRaw { events, ack } => {
            let _ = ack.send(sess.submit_events(&events).await);
            false
        }
        #[cfg(any(test, feature = "test-util"))]
        CaptureCmd::Block { entered, release } => {
            let _ = entered.send(());
            // Prefer crash over release so crash_kill is not raced by a
            // subsequent release send (D2).
            tokio::select! {
                biased;
                _ = crash_rx.changed() => *crash_rx.borrow(),
                _ = release => false,
            }
        }
        CaptureCmd::Shutdown(ack) => {
            if let Some(ack) = ack {
                let _ = ack.send(());
            }
            false
        }
    }
}

async fn submit_mapped(
    session: &mut LhcSession,
    mapped: Vec<MappedEvent>,
) -> Result<lhc::intake_stream::BatchResult, String> {
    if mapped.is_empty() {
        return Ok(lhc::intake_stream::BatchResult {
            events: vec![],
            turn_transitions: vec![],
            queued_work: vec![],
            thread_position: lhc::intake_stream::ThreadPosition {
                last_event_order: session.generation as i64,
            },
        });
    }
    let inputs: Vec<_> = mapped.into_iter().map(|e| e.input).collect();
    session.submit_events(&inputs).await
}
