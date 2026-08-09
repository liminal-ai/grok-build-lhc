//! Chunk 1 certification — exact counts and key-set equality.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use grok_lhc_host::{
    CaptureHandle, CaptureOpenState, CaptureOpenWaitError, CompactEventBridge, CompactMode,
    ContextEngine, CountingLhcInferenceSampler, LhcFileConfig, LhcInferenceRequest,
    MockLhcInferenceSampler, ServeDecision, SourceKindIndex, apply_resolved_config,
    apply_serve_decision, body_has_tool_cycle, build_writeback_conversation, capture_active,
    capture_archive_ready, capture_model_or_thinking_change, clear_last_serve_outcome,
    clear_open_hold_for_test, compare_serve_equivalence, decide_substitution,
    encode_session_id_for_path, equivalence_snapshot, execute_repair, format_status_report,
    health_check, inference_sampler_registered, informational_hit_count, is_enabled,
    last_serve_outcome, lookup_session, native_prompt_indices, observe_serve_equivalence,
    paths_disagree, plan_repair, preview_call_count, project_conversation_canonical,
    replace_call_count, reset_compact_call_counters, reset_equivalence_counters,
    resolve_compact_mode, resolve_lhc_config, serve_compared_turns, serve_fallback_turns,
    serve_request_context, set_compact_mode_for_test, set_force_classify_list_failure,
    set_open_hold_for_test, shadow_preview_compact, shutdown_session, spawn_capture, status_report,
    structural_hit_count, tee_chat_persistence, thread_file_path, wait_capture_archive_ready,
};
use lhc::intake_stream::{BatchEventOutcome, BatchSkipReason, EventRecord};
use lhc::shared_tech::view::{
    SessionAssistantMessage, SessionAssistantPart, SessionAssistantPartType,
    SessionModelChangeEntry, SessionThreadView, SessionThreadViewEntry,
    SessionThreadViewEntrySource, SessionThreadViewMessage, SessionThreadViewRuntimeEntry,
    SessionToolResultMessage, SessionUserMessage,
};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use xai_chat_state::{
    ChatStateActor, NullChatPersistence, PersistenceRecord, estimate_conversation_tokens,
};
use xai_grok_sampling_types::{ConversationItem, SamplingConfig, SyntheticReason, ToolCall};

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn keys(events: &[EventRecord]) -> BTreeSet<String> {
    events
        .iter()
        .map(|e| e.idempotency_key().to_string())
        .collect()
}

fn wait_events(handle: &CaptureHandle, min: usize) -> Vec<EventRecord> {
    for _ in 0..200 {
        match handle.list_events_blocking() {
            Ok(ev) if ev.len() >= min => return ev,
            Ok(_) | Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
    handle
        .list_events_blocking()
        .expect("list_events after wait")
}

fn wait_registry_gone(session_id: &str) {
    for _ in 0..100 {
        if !capture_active(session_id) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("registry entry still present for {session_id}");
}

fn wait_exact(handle: &CaptureHandle, n: usize) -> Vec<EventRecord> {
    for _ in 0..200 {
        if let Ok(ev) = handle.list_events_blocking()
            && ev.len() == n
        {
            return ev;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let ev = handle.list_events_blocking().unwrap();
    assert_eq!(ev.len(), n, "expected exact event count");
    ev
}

#[test]
fn gate_off_by_default() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    unsafe { std::env::remove_var("GROK_LHC") };
    assert!(!is_enabled());
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
}

#[test]
fn disabled_tee_is_passthrough_no_registry() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::remove_var("GROK_LHC");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "cert-disabled";
    let _p = tee_chat_persistence(sid, "/tmp", &[], Box::new(NullChatPersistence), None);
    assert!(!capture_active(sid));
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

#[test]
fn live_capture_and_idempotent_replay() {
    let root = TempDir::new().unwrap();
    let sid = "cert-replay";
    let history = [
        ConversationItem::user("one"),
        ConversationItem::assistant("two"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&history[0]);
    handle.persist(&history[1]);
    handle.flush_blocking();
    let first = wait_exact(&handle, 3);
    let k1 = keys(&first);
    handle.shutdown_blocking();
    wait_registry_gone(sid);

    let handle2 = spawn_capture(sid, Some("/tmp"), &history, Some(root.path()), None).unwrap();
    let second = wait_exact(&handle2, 3);
    assert_eq!(keys(&second), k1);
    handle2.shutdown_blocking();
}

#[test]
fn restart_reuses_thread_and_skips_duplicate_bootstrap() {
    let root = TempDir::new().unwrap();
    let sid = "cert-restart";
    let history = [
        ConversationItem::user("prompt"),
        ConversationItem::assistant("answer"),
    ];
    let first_keys = {
        let handle = spawn_capture(sid, Some("/tmp"), &history, Some(root.path()), None).unwrap();
        let ev = wait_exact(&handle, 3);
        let k = keys(&ev);
        handle.shutdown_blocking();
        wait_registry_gone(sid);
        k
    };
    let handle2 = spawn_capture(sid, Some("/tmp"), &history, Some(root.path()), None).unwrap();
    let ev2 = wait_exact(&handle2, 3);
    assert_eq!(keys(&ev2), first_keys);
    handle2.shutdown_blocking();
}

#[test]
fn session_fork_transcripts_are_independent_on_shared_root() {
    let root = TempDir::new().unwrap();
    let history = [
        ConversationItem::user("shared"),
        ConversationItem::assistant("ok"),
    ];
    let a = spawn_capture("fork-a", Some("/tmp"), &history, Some(root.path()), None).unwrap();
    let b = spawn_capture("fork-b", Some("/tmp"), &history, Some(root.path()), None).unwrap();
    let ea = wait_exact(&a, 3);
    let eb = wait_exact(&b, 3);
    assert!(keys(&ea).is_disjoint(&keys(&eb)));

    a.persist(&ConversationItem::user("only-a"));
    a.flush_blocking();
    let ea2 = wait_exact(&a, 4);
    assert_eq!(b.list_events_blocking().unwrap().len(), 3);

    let ka = keys(&ea2);
    let kb = keys(&eb);
    a.shutdown_blocking();
    b.shutdown_blocking();
    wait_registry_gone("fork-a");
    wait_registry_gone("fork-b");

    let a2 = spawn_capture("fork-a", Some("/tmp"), &history, Some(root.path()), None).unwrap();
    let b2 = spawn_capture("fork-b", Some("/tmp"), &history, Some(root.path()), None).unwrap();
    assert_eq!(keys(&wait_exact(&a2, 4)), ka);
    assert_eq!(keys(&wait_exact(&b2, 3)), kb);
    a2.shutdown_blocking();
    b2.shutdown_blocking();
}

/// B1 case 1 — N prune-shaped replaces add zero events.
#[test]
fn prune_shaped_replace_history_adds_zero_events() {
    let root = TempDir::new().unwrap();
    let sid = "cert-prune-replace";
    let items = [
        ConversationItem::user("u1"),
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        ConversationItem::assistant("a2"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &items, Some(root.path()), None).unwrap();
    let before = wait_exact(&handle, 6);
    let before_keys = keys(&before);

    for _ in 0..5 {
        handle.replace_history(&items[..2]); // prune-shaped: only removals
    }
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let after = handle.list_events_blocking().unwrap();
    assert_eq!(after.len(), 6);
    assert_eq!(keys(&after), before_keys);
    handle.shutdown_blocking();
}

/// B1 case 2 — repair synthetic ToolResult is recorded; nothing else.
#[test]
fn replace_history_records_repair_tool_result_only() {
    let root = TempDir::new().unwrap();
    let sid = "cert-repair";
    let user = ConversationItem::user("go");
    let assistant = ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
        content: "calling".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }],
        model_id: None,
        model_fingerprint: None,
        reasoning_effort: None,
    });
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&user);
    handle.persist(&assistant);
    handle.flush_blocking();
    // user + assistant_text + tool_call = 3 (no turn_end with tools)
    let before = wait_exact(&handle, 3);
    let before_keys = keys(&before);

    let repair = ConversationItem::tool_result("c1", "repaired");
    let repaired = vec![user, assistant, repair];
    handle.replace_history(&repaired);
    handle.flush_blocking();
    let after = wait_events(&handle, 4);
    assert_eq!(after.len(), 4);
    let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
    assert_eq!(new_keys.len(), 1);
    let repair_ev = after
        .iter()
        .find(|e| e.event_kind().as_str() == "tool_result")
        .expect("repair tool_result recorded");
    let payload = repair_ev
        .tool_result_payload()
        .expect("tool_result payload");
    assert_eq!(payload.tool_call_id, "c1");
    assert!(
        payload.content.contains("repaired"),
        "content={:?}",
        payload.content
    );
    handle.shutdown_blocking();
}

/// B1 case 3 — reminder System prepend is recorded; nothing else.
#[test]
fn replace_history_records_injected_system_reminder_only() {
    let root = TempDir::new().unwrap();
    let sid = "cert-reminder";
    let items = [
        ConversationItem::user("u"),
        ConversationItem::assistant("a"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &items, Some(root.path()), None).unwrap();
    let before = wait_exact(&handle, 3);
    let before_keys = keys(&before);

    let mut with_reminder = vec![ConversationItem::system("memory reminder")];
    with_reminder.extend(items);
    handle.replace_history(&with_reminder);
    handle.flush_blocking();
    let after = wait_events(&handle, 4);
    assert_eq!(after.len(), 4);
    let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
    assert_eq!(new_keys.len(), 1);
    assert!(after.iter().any(|e| {
        e.event_kind().as_str() == "runtime_note"
            && e.text_payload()
                .is_some_and(|p| p.text.contains("memory reminder"))
    }));
    handle.shutdown_blocking();
}

/// B1 case 4 — CompactionMeta summary recorded; survivors not re-recorded.
#[test]
fn replace_history_records_compaction_meta_only() {
    let root = TempDir::new().unwrap();
    let sid = "cert-compact-meta";
    let full = [
        ConversationItem::user("u1"),
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        ConversationItem::assistant("a2"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &full, Some(root.path()), None).unwrap();
    let before = wait_exact(&handle, 6);
    let before_keys = keys(&before);

    let mut summary = ConversationItem::user("compacted summary");
    if let ConversationItem::User(u) = &mut summary {
        u.synthetic_reason = Some(SyntheticReason::CompactionMeta);
    }
    let compacted = vec![summary, full[3].clone()];
    handle.replace_history(&compacted);
    handle.flush_blocking();
    let after = wait_events(&handle, 7);
    assert_eq!(after.len(), 7, "exactly one new summary event");
    let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
    assert_eq!(new_keys.len(), 1);
    let summary_ev = after
        .iter()
        .find(|e| new_keys.contains(e.idempotency_key()))
        .expect("new summary event");
    assert_eq!(summary_ev.event_kind().as_str(), "runtime_note");
    assert!(
        summary_ev
            .text_payload()
            .is_some_and(|p| p.text.contains("compacted summary")),
        "summary text missing"
    );
    // Survivor a2 must not have been re-keyed under a new key.
    assert!(before_keys.is_subset(&keys(&after)));
    handle.shutdown_blocking();
}

/// J1 — identical-content summary dedup is inert (correct by design).
/// A byte-identical summary yields no new event, does not disturb the key
/// stream for subsequent items, and leaves the occurrence walk aligned.
#[test]
fn identical_content_summary_dedup_is_inert() {
    let root = TempDir::new().unwrap();
    let sid = "cert-identical-summary-dedup";
    let survivor = ConversationItem::assistant("survivor-a");
    let summary = ConversationItem::user_meta("byte-identical summary text");
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&ConversationItem::user("seed"));
    handle.persist(&survivor);
    handle.flush_blocking();
    let _ = wait_events(&handle, 1);

    // First write-back-shaped replace: summary is new.
    handle.replace_history(&[summary.clone(), survivor.clone()]);
    handle.flush_blocking();
    let after_first = wait_events(&handle, 1);
    let keys_after_summary = keys(&after_first);
    let summary_keys: Vec<_> = after_first
        .iter()
        .filter(|e| {
            e.text_payload()
                .is_some_and(|p| p.text.contains("byte-identical summary text"))
        })
        .map(|e| e.idempotency_key().to_string())
        .collect();
    assert_eq!(
        summary_keys.len(),
        1,
        "summary must record exactly once on first replace"
    );

    // Second replace: same summary (byte-identical) + survivor + new live user.
    let live = ConversationItem::user("live-after-dedup");
    handle.replace_history(&[summary.clone(), survivor.clone(), live.clone()]);
    handle.flush_blocking();
    let after_second = wait_events(&handle, after_first.len() + 1);
    let summary_hits = after_second
        .iter()
        .filter(|e| {
            e.text_payload()
                .is_some_and(|p| p.text.contains("byte-identical summary text"))
        })
        .count();
    assert_eq!(
        summary_hits, 1,
        "byte-identical summary must yield no new event"
    );
    // Key stream for prior items undisturbed.
    assert!(
        keys_after_summary.is_subset(&keys(&after_second)),
        "prior keys must remain; dedup must not reshuffle the stream"
    );
    let new_keys: BTreeSet<_> = keys(&after_second)
        .difference(&keys_after_summary)
        .cloned()
        .collect();
    assert!(
        !new_keys.is_empty(),
        "subsequent live user must still record"
    );
    assert!(
        after_second.iter().any(|e| {
            e.text_payload()
                .is_some_and(|p| p.text.contains("live-after-dedup"))
        }),
        "live user text missing"
    );

    // Occurrence walk aligned: replace again with the same body — no growth.
    let len_stable = after_second.len();
    let keys_stable = keys(&after_second);
    handle.replace_history(&[summary, survivor, live]);
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let again = handle.list_events_blocking().unwrap();
    assert_eq!(
        again.len(),
        len_stable,
        "third identical replace must add nothing"
    );
    assert_eq!(
        keys(&again),
        keys_stable,
        "occurrence walk must stay aligned"
    );
    handle.shutdown_blocking();
}

/// B1 case 5 — rewind then re-send identical B, including across restart.
#[test]
fn rewind_then_reappend_identical_item_is_recorded_across_restart() {
    let root = TempDir::new().unwrap();
    let sid = "cert-rewind-reappend";
    let a = ConversationItem::user("a");
    let b = ConversationItem::assistant("b");
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&a);
    handle.persist(&b);
    handle.flush_blocking();
    let before = wait_exact(&handle, 3);
    let before_keys = keys(&before);

    handle.replace_history(std::slice::from_ref(&a));
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        handle.list_events_blocking().unwrap().len(),
        3,
        "rewind must not re-record A"
    );

    handle.persist(&b);
    handle.flush_blocking();
    let after = wait_events(&handle, 5);
    assert_eq!(after.len(), 5);
    let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
    assert_eq!(new_keys.len(), 2, "re-sent B → assistant_text + turn_end");

    let mid_keys = keys(&after);
    handle.shutdown_blocking();
    wait_registry_gone(sid);

    // Restart with post-rewind conversation [A] only — tracker must seed from LHC
    // (which still has B), so another identical B is still a new occurrence.
    let handle2 = spawn_capture(
        sid,
        Some("/tmp"),
        std::slice::from_ref(&a),
        Some(root.path()),
        None,
    )
    .unwrap();
    let resumed = wait_exact(&handle2, 5);
    assert_eq!(keys(&resumed), mid_keys);
    handle2.persist(&b);
    handle2.flush_blocking();
    let again = wait_events(&handle2, 7);
    assert_eq!(
        again.len(),
        7,
        "post-restart re-send of B must record again"
    );
    handle2.shutdown_blocking();
}

#[test]
fn concurrency_identical_digest_path() {
    let root = TempDir::new().unwrap();
    let sid = "cert-conc";
    let handle = Arc::new(spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap());
    let item = ConversationItem::user("same");
    let mut joins = Vec::new();
    for _ in 0..8 {
        let h = Arc::clone(&handle);
        let it = item.clone();
        joins.push(thread::spawn(move || h.persist(&it)));
    }
    for j in joins {
        j.join().unwrap();
    }
    handle.flush_blocking();
    let ev = wait_exact(&handle, 8);
    assert_eq!(keys(&ev).len(), 8);
    handle.shutdown_blocking();
}

/// B3 — genuine mid-batch crash: worker dies with queued work uncommitted.
#[test]
fn crash_mid_batch_no_duplication_on_rerun() {
    let root = TempDir::new().unwrap();
    let sid = "cert-crash";
    let items = [
        ConversationItem::user("u1"),
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        ConversationItem::assistant("a2"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let (_release_tx, release_rx) = oneshot::channel();
    let entered = handle.block_worker(release_rx);
    let _ = entered.blocking_recv();
    for item in &items {
        handle.persist(item);
    }
    // Kill without releasing the blocker — do not send release (it can race
    // the crash arm and calmly drain the queue).
    handle.crash_kill();
    wait_registry_gone(sid);
    // Wait until the detached worker is gone (thread file lock released).
    thread::sleep(Duration::from_millis(200));

    // Discriminating observation: empty bootstrap must see 0 events —
    // proves queued work never committed (a calm drain would leave 6).
    let probe = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    thread::sleep(Duration::from_millis(100));
    let empty = probe.list_events_blocking().unwrap();
    assert_eq!(
        empty.len(),
        0,
        "crash must leave thread empty; got {} events",
        empty.len()
    );
    probe.shutdown_blocking();
    wait_registry_gone(sid);

    let handle2 = spawn_capture(sid, Some("/tmp"), &items, Some(root.path()), None).unwrap();
    let final_ev = wait_exact(&handle2, 6);
    assert_eq!(final_ev.len(), 6);
    assert_eq!(keys(&final_ev).len(), 6);
    let k = keys(&final_ev);
    handle2.shutdown_blocking();
    wait_registry_gone(sid);

    let handle3 = spawn_capture(sid, Some("/tmp"), &items, Some(root.path()), None).unwrap();
    let again = wait_exact(&handle3, 6);
    assert_eq!(keys(&again), k);
    handle3.shutdown_blocking();
}

#[test]
fn poisoned_lhc_disables_and_host_continues() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "cert-poison";
    let (mock, mut rx) = xai_chat_state::MockChatPersistence::new();
    let mut tee = tee_chat_persistence(sid, "/tmp", &[], Box::new(mock), None);
    let handle = lookup_session(sid).unwrap();
    handle.poison_blocking();
    for i in 0..5 {
        tee.persist_message(&ConversationItem::user(format!("still-{i}")));
    }
    tee.flush();
    let mut disabled = false;
    for _ in 0..80 {
        if handle.capture_disabled_blocking() {
            disabled = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(disabled);
    assert!(
        rx.drain()
            .iter()
            .filter(|r| matches!(r, PersistenceRecord::Message(_)))
            .count()
            >= 5
    );
    drop(tee);
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

#[test]
fn model_toggle_cycle_records_all_transitions() {
    let root = TempDir::new().unwrap();
    let sid = "cert-model-toggle";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.model_change("m1", "m1", "none", "high");
    handle.model_change("m1", "m1", "high", "none");
    handle.model_change("m1", "m1", "none", "high");
    handle.flush_blocking();
    assert_eq!(wait_exact(&handle, 3).len(), 3);
    handle.shutdown_blocking();
    wait_registry_gone(sid);
    let handle2 = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle2.model_change("m1", "m1", "high", "none");
    handle2.flush_blocking();
    assert_eq!(wait_exact(&handle2, 4).len(), 4);
    handle2.shutdown_blocking();
}

#[test]
fn model_noop_suppressed_and_previous_is_real() {
    let root = TempDir::new().unwrap();
    let sid = "cert-model-noop";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.model_change("gpt-a", "gpt-a", "none", "none");
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(100));
    assert!(handle.list_events_blocking().unwrap().is_empty());
    handle.model_change("gpt-a", "gpt-b", "none", "none");
    handle.flush_blocking();
    let ev = wait_exact(&handle, 1);
    let payload = ev[0].model_change_payload().unwrap();
    assert_eq!(payload.previous_model, "gpt-a");
    assert_eq!(payload.new_model, "gpt-b");
    handle.shutdown_blocking();
}

#[test]
fn model_toggle_survives_colon_session_id() {
    let root = TempDir::new().unwrap();
    let sid = "acp:sess:with:colons";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.model_change("m", "m", "none", "high");
    handle.model_change("m", "m", "high", "none");
    handle.flush_blocking();
    assert_eq!(wait_exact(&handle, 2).len(), 2);
    handle.shutdown_blocking();
    wait_registry_gone(sid);
    let handle2 = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle2.model_change("m", "m", "none", "high");
    handle2.flush_blocking();
    assert_eq!(wait_exact(&handle2, 3).len(), 3);
    handle2.shutdown_blocking();
}

#[test]
fn batch_skip_reports_duplicate_idempotency_key() {
    let root = TempDir::new().unwrap();
    let sid = "cert-dup-skip";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let item = ConversationItem::user("dup");
    let (mapped, _) = grok_lhc_host::map_history(
        sid,
        0,
        std::slice::from_ref(&item),
        &grok_lhc_host::TurnEndFacts::default(),
    );
    handle.persist(&item);
    handle.flush_blocking();
    let _ = wait_exact(&handle, 1);
    let inputs: Vec<_> = mapped.into_iter().map(|e| e.input).collect();
    let batch = handle.submit_raw_blocking(inputs).expect("resubmit");
    for entry in &batch.events {
        assert_eq!(entry.outcome, BatchEventOutcome::Skipped);
        assert_eq!(
            entry.skip_reason,
            Some(BatchSkipReason::DuplicateIdempotencyKey)
        );
    }
    handle.shutdown_blocking();
}

/// B6/A3 — drain via watch branch only.
#[test]
fn teardown_drains_via_watch_branch() {
    let root = TempDir::new().unwrap();
    let sid = "cert-teardown-watch";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let entered = handle.block_worker(release_rx);
    let _ = entered.blocking_recv();
    for i in 0..5 {
        handle.persist(&ConversationItem::user(format!("queued-{i}")));
    }
    handle.shutdown_watch_only();
    let _ = release_tx.send(());
    wait_registry_gone(sid);
    let handle2 = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    assert_eq!(wait_exact(&handle2, 5).len(), 5);
    handle2.shutdown_blocking();
}

/// B6/A3 — drain via Shutdown cmd branch only.
#[test]
fn teardown_drains_via_shutdown_cmd_branch() {
    let root = TempDir::new().unwrap();
    let sid = "cert-teardown-cmd";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let entered = handle.block_worker(release_rx);
    let _ = entered.blocking_recv();
    for i in 0..5 {
        handle.persist(&ConversationItem::user(format!("cmdq-{i}")));
    }
    handle.shutdown_cmd_only();
    let _ = release_tx.send(());
    wait_registry_gone(sid);
    let handle2 = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    assert_eq!(wait_exact(&handle2, 5).len(), 5);
    handle2.shutdown_blocking();
}

#[test]
fn teardown_from_drop_clears_registry_without_panic() {
    let root = TempDir::new().unwrap();
    let sid = "cert-teardown";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&ConversationItem::user("x"));
    handle.flush_blocking();
    let _ = wait_exact(&handle, 1);
    handle.shutdown_async();
    wait_registry_gone(sid);
    shutdown_session(sid);
}

#[test]
fn queue_saturation_drops_and_counts_model_exactly() {
    let root = TempDir::new().unwrap();
    let sid = "cert-saturate";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let entered = handle.block_worker(release_rx);
    let _ = entered.blocking_recv();

    let flood = grok_lhc_host::CAPTURE_QUEUE_CAP + 64;
    for i in 0..flood {
        handle.persist(&ConversationItem::user(format!("flood-{i}")));
    }
    assert!(handle.dropped_count() > 0);

    let before = handle.dropped_count();
    let model_flood = 32usize;
    for _ in 0..model_flood {
        handle.model_change("a", "b", "none", "high");
    }
    let after = handle.dropped_count();
    assert_eq!(
        after - before,
        model_flood as u64,
        "every model_change drop must be counted"
    );

    let _ = release_tx.send(());
    handle.shutdown_blocking();
}

#[test]
fn aborted_tool_turn_closed_by_synthetic_wake() {
    let root = TempDir::new().unwrap();
    let sid = "cert-abort-wake";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&ConversationItem::user("go"));
    handle.persist(&ConversationItem::Assistant(
        xai_grok_sampling_types::AssistantItem {
            content: "calling".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        },
    ));
    let mut wake = ConversationItem::user("task done");
    if let ConversationItem::User(u) = &mut wake {
        u.synthetic_reason = Some(SyntheticReason::TaskCompleted);
    }
    handle.persist(&wake);
    handle.flush_blocking();
    let ev = wait_events(&handle, 3);
    let kinds: Vec<_> = ev.iter().map(|e| e.event_kind().as_str()).collect();
    assert!(kinds.contains(&"turn_end"), "got {kinds:?}");
    handle.shutdown_blocking();
}

/// B4 — distinct sessions with colliding sanitized names get distinct files.
#[test]
fn session_ids_differing_by_sanitized_char_get_distinct_threads() {
    let root = TempDir::new().unwrap();
    let a = "a:b";
    let b = "a_b";
    assert_ne!(encode_session_id_for_path(a), encode_session_id_for_path(b));
    let pa = thread_file_path(root.path(), a);
    let pb = thread_file_path(root.path(), b);
    assert_ne!(pa, pb);

    let ha = spawn_capture(a, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let hb = spawn_capture(b, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    ha.persist(&ConversationItem::user("from-colon"));
    hb.persist(&ConversationItem::user("from-underscore"));
    ha.flush_blocking();
    hb.flush_blocking();
    let ea = wait_exact(&ha, 1);
    let eb = wait_exact(&hb, 1);
    assert!(keys(&ea).is_disjoint(&keys(&eb)));
    assert!(pa.exists());
    assert!(pb.exists());
    ha.shutdown_blocking();
    hb.shutdown_blocking();
}

/// B5 — orphan thread file without registry binding is refused.
#[test]
fn orphan_thread_file_without_registry_is_refused() {
    let root = TempDir::new().unwrap();
    let sid = "cert-orphan";
    // Create a legitimate session, then wipe the registry while keeping the file.
    {
        let h = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
        h.persist(&ConversationItem::user("x"));
        h.flush_blocking();
        let _ = wait_exact(&h, 1);
        h.shutdown_blocking();
        wait_registry_gone(sid);
    }
    let registry = root.path().join("registry.sqlite");
    assert!(registry.exists());
    std::fs::remove_file(&registry).unwrap();
    // Also remove WAL/SHM if present.
    let _ = std::fs::remove_file(root.path().join("registry.sqlite-wal"));
    let _ = std::fs::remove_file(root.path().join("registry.sqlite-shm"));
    assert!(thread_file_path(root.path(), sid).exists());
    // Spawn must not block; refused open unregisters asynchronously.
    let _handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None);
    wait_registry_gone(sid);
    assert!(
        !capture_active(sid),
        "orphan file must refuse open (capture_active clear)"
    );
}

/// B5 — path disagreement policy is refuse (unit).
#[test]
fn path_disagreement_policy_refuses() {
    assert!(paths_disagree(
        std::path::Path::new("/tmp/threads/grok-s.sqlite"),
        "/tmp/threads/grok-OTHER.sqlite"
    ));
    assert!(!paths_disagree(
        std::path::Path::new("/tmp/threads/grok-s.sqlite"),
        "/tmp/threads/grok-s.sqlite"
    ));
}

/// B2 — list_events failure at open refuses the session.
#[test]
fn list_events_failure_refuses_open() {
    let root = TempDir::new().unwrap();
    let sid = "cert-listevents-fail";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&ConversationItem::user("x"));
    handle.flush_blocking();
    let _ = wait_exact(&handle, 1);
    handle.shutdown_blocking();
    wait_registry_gone(sid);

    let thread = thread_file_path(root.path(), sid);
    sqlite_exec(&thread, "PRAGMA foreign_keys=OFF; DROP TABLE event;");

    // Spawn must not block; B2 refuse is observable via unregister.
    let _handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None);
    wait_registry_gone(sid);
    assert!(
        !capture_active(sid),
        "list_events failure must refuse open (capture_active clear)"
    );
}

/// B2 — seed_from_db surfaces list_events Err (open maps it to refuse).
#[test]
fn seed_from_db_errors_when_list_events_fails() {
    let root = TempDir::new().unwrap();
    let sid = "cert-seed-poison";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.shutdown_blocking();
    wait_registry_gone(sid);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut session = rt
        .block_on(grok_lhc_host::session_open_for_test(sid, root.path()))
        .expect("open for seed test");
    session.poison();
    assert!(rt.block_on(session.seed_from_db()).is_err());
}

fn registry_sqlite(root: &std::path::Path) -> std::path::PathBuf {
    root.join("registry.sqlite")
}

fn sqlite_exec(db: &std::path::Path, sql: &str) {
    let conn = rusqlite::Connection::open(db).expect("open sqlite");
    conn.execute_batch(sql)
        .unwrap_or_else(|e| panic!("sql {sql}: {e}"));
}

/// B5 — registry file_path disagrees with session layout → refuse.
#[test]
fn registry_file_path_disagreement_refuses_open() {
    let root = TempDir::new().unwrap();
    let sid = "cert-disagree";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&ConversationItem::user("x"));
    handle.flush_blocking();
    let _ = wait_exact(&handle, 1);
    handle.shutdown_blocking();
    wait_registry_gone(sid);

    let expected = thread_file_path(root.path(), sid);
    let decoy = root.path().join("threads").join("grok-decoy.sqlite");
    std::fs::copy(&expected, &decoy).unwrap();
    let decoy_str = decoy.to_string_lossy().replace('\'', "''");
    sqlite_exec(
        &registry_sqlite(root.path()),
        &format!("UPDATE threads SET file_path = '{decoy_str}';"),
    );

    // Spawn must not block; B5 refuse is observable via unregister.
    let _handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None);
    wait_registry_gone(sid);
    assert!(
        !capture_active(sid),
        "registry/file disagreement must refuse (capture_active clear)"
    );
}

/// B5 — resolve(thread_id) fails but list_threads matches file_path → reopen.
#[test]
fn list_threads_file_path_fallback_reopens() {
    let root = TempDir::new().unwrap();
    let sid = "cert-list-fallback";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&ConversationItem::user("keep-me"));
    handle.flush_blocking();
    let before = wait_exact(&handle, 1);
    let before_keys = keys(&before);
    handle.shutdown_blocking();
    wait_registry_gone(sid);

    // Break resolve-by-id while leaving the file_path row for list_threads.
    sqlite_exec(
        &registry_sqlite(root.path()),
        "UPDATE threads SET thread_id = 'not-the-file-id';",
    );

    let handle2 = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None)
        .expect("list_threads file_path fallback must reopen");
    // Successful open keeps the registry entry live.
    for _ in 0..100 {
        if capture_active(sid) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(capture_active(sid), "fallback reopen must stay registered");
    let after = wait_exact(&handle2, 1);
    assert_eq!(keys(&after), before_keys);
    handle2.shutdown_blocking();
}

fn with_lhc_env(root: &std::path::Path, f: impl FnOnce()) {
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root);
    }
    f();
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// C1 — host installs the tee from a **current-thread** runtime (shipping
/// `spawn_session_actor` shape). Must return normally; timeout fails hangs.
#[tokio::test(flavor = "current_thread")]
// `env_lock` is a std guard spanning the timeout's await. The awaited body is
// entirely synchronous (`with_lhc_env` runs a sync closure), so the guard never
// spans a real suspension point and cannot deadlock.
#[allow(clippy::await_holding_lock)]
async fn tee_from_async_context_does_not_block_or_panic() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-async-tee-spawn";
    tokio::time::timeout(Duration::from_secs(10), async {
        with_lhc_env(root.path(), || {
            let tee = tee_chat_persistence(
                sid,
                "/tmp",
                &[],
                Box::new(xai_chat_state::NullChatPersistence),
                None,
            );
            for _ in 0..100 {
                if capture_active(sid) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(capture_active(sid), "successful open should register");
            drop(tee);
            wait_registry_gone(sid);
        });
    })
    .await
    .expect("tee install blocked the current-thread runtime (C1)");
}

/// C1 — dropping the tee inside a multi-thread async runtime must not panic.
#[tokio::test(flavor = "multi_thread")]
// See `tee_from_async_context_does_not_block_or_panic` — synchronous body.
#[allow(clippy::await_holding_lock)]
async fn tee_drop_from_async_context_does_not_block_or_panic() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-async-tee-drop";
    tokio::time::timeout(Duration::from_secs(10), async {
        with_lhc_env(root.path(), || {
            let tee = tee_chat_persistence(
                sid,
                "/tmp",
                &[],
                Box::new(xai_chat_state::NullChatPersistence),
                None,
            );
            for _ in 0..100 {
                if capture_active(sid) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(capture_active(sid));
            drop(tee);
            wait_registry_gone(sid);
            assert!(!capture_active(sid));
        });
    })
    .await
    .expect("tee drop blocked the async runtime (C1)");
}

/// D1 — public hook-3 entry records a change for the matching session id.
/// Runs under tokio (host `apply` is async). Blocking helpers go through
/// `spawn_blocking` so they cannot reintroduce a C1-style runtime panic.
#[tokio::test(flavor = "multi_thread")]
async fn capture_model_entry_records_for_matching_session() {
    let root = TempDir::new().unwrap();
    let sid = "cert-hook3-match";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    // None effort → "none" normalization at the public entry (sync, non-blocking).
    capture_model_or_thinking_change(sid, "m1", "m1", None, Some("high"));
    let handle2 = handle.clone();
    let ev = tokio::task::spawn_blocking(move || {
        handle2.flush_blocking();
        wait_exact(&handle2, 1)
    })
    .await
    .unwrap();
    assert_eq!(ev[0].event_kind().as_str(), "thinking_level_change");
    let p = ev[0].thinking_level_change_payload().unwrap();
    assert_eq!(p.previous_level, "none");
    assert_eq!(p.new_level, "high");
    tokio::task::spawn_blocking(move || handle.shutdown_blocking())
        .await
        .unwrap();
}

/// D1 — wrong session id at hook 3 is silently discarded.
#[test]
fn capture_model_entry_ignores_mismatched_session_id() {
    let root = TempDir::new().unwrap();
    let sid = "cert-hook3-right";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    capture_model_or_thinking_change("cert-hook3-WRONG", "a", "b", None, Some("high"));
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(100));
    assert!(
        handle.list_events_blocking().unwrap().is_empty(),
        "mismatched session id must not record"
    );
    handle.shutdown_blocking();
}

/// D1 — unregistered session id is a no-op (lookup miss / fast-gate safe).
#[test]
fn capture_model_entry_noop_when_no_capture_active() {
    // Do not assert process-wide `any_capture_active()` — other tests may hold
    // captures in parallel. An id that is not registered must still no-op.
    let sid = "cert-hook3-nobody-never-registered";
    assert!(lookup_session(sid).is_none());
    capture_model_or_thinking_change(sid, "a", "b", None, Some("high"));
    assert!(lookup_session(sid).is_none());
}

/// D1 — no-op suppression at the public entry (same model + same level).
#[test]
fn capture_model_entry_suppresses_noop_transition() {
    let root = TempDir::new().unwrap();
    let sid = "cert-hook3-noop";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    capture_model_or_thinking_change(sid, "m", "m", None, None);
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(100));
    assert!(handle.list_events_blocking().unwrap().is_empty());
    handle.shutdown_blocking();
}

/// D3 — after refused open, tee stops clone/warn and still reaches inner.
#[test]
fn refused_open_tee_stops_and_host_persists() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-d3-refuse-tee";
    // Seed a thread file, then wipe registry so reopen refuses (B5 orphan).
    {
        let h = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
        h.persist(&ConversationItem::user("seed"));
        h.flush_blocking();
        let _ = wait_exact(&h, 1);
        h.shutdown_blocking();
        wait_registry_gone(sid);
    }
    std::fs::remove_file(root.path().join("registry.sqlite")).unwrap();
    let _ = std::fs::remove_file(root.path().join("registry.sqlite-wal"));
    let _ = std::fs::remove_file(root.path().join("registry.sqlite-shm"));

    let (mock, mut rx) = xai_chat_state::MockChatPersistence::new();
    with_lhc_env(root.path(), || {
        let mut tee = tee_chat_persistence(sid, "/tmp", &[], Box::new(mock), None);
        // Probe the shared handle before/while open refuses.
        let probe = lookup_session(sid);
        wait_registry_gone(sid);
        for _ in 0..50 {
            if probe.as_ref().is_some_and(|h| h.is_closed()) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let probe = probe.expect("spawn returned a handle before refuse");
        assert!(probe.is_closed(), "refused open must close the channel");
        let drops_before = probe.dropped_count();

        for i in 0..8 {
            tee.persist_message(&ConversationItem::user(format!("post-refuse-{i}")));
        }
        tee.flush();

        assert_eq!(
            probe.dropped_count(),
            drops_before,
            "stopped tee must not note_drop per item"
        );
        let msgs: Vec<_> = rx
            .drain()
            .into_iter()
            .filter(|r| matches!(r, xai_chat_state::PersistenceRecord::Message(_)))
            .collect();
        assert!(
            msgs.len() >= 8,
            "inner persistence must still receive messages, got {}",
            msgs.len()
        );
        drop(tee);
    });
}

/// D5 — drive all four ChatPersistence methods through the tee itself.
#[tokio::test(flavor = "multi_thread")]
// See `tee_from_async_context_does_not_block_or_panic` — synchronous body.
#[allow(clippy::await_holding_lock)]
async fn tee_chat_persistence_methods_reach_inner_and_lhc() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-tee-methods";
    tokio::time::timeout(Duration::from_secs(15), async {
        with_lhc_env(root.path(), || {
            let (mock, mut rx) = xai_chat_state::MockChatPersistence::new();
            let mut tee = tee_chat_persistence(sid, "/tmp", &[], Box::new(mock), None);
            for _ in 0..100 {
                if capture_active(sid) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(capture_active(sid));
            let handle = lookup_session(sid).expect("registered");

            tee.persist_message(&ConversationItem::user("via-tee"));
            let cwd_item = ConversationItem::user("cwd-switch");
            let _ack = tee.persist_working_directory_switch_and_ack(&cwd_item);
            tee.replace_history(&[
                ConversationItem::user("replaced-a"),
                ConversationItem::assistant("replaced-b"),
            ]);
            tee.flush();
            let handle2 = handle.clone();
            let ev = tokio::task::block_in_place(|| {
                handle2.flush_blocking();
                wait_events(&handle2, 1)
            });
            assert!(
                ev.iter().any(
                    |e| e.text_payload().is_some_and(|p| p.text.contains("via-tee"))
                        || e.text_payload()
                            .is_some_and(|p| p.text.contains("replaced"))
                ),
                "LHC should see tee-driven events"
            );

            let records = rx.drain();
            assert!(
                records
                    .iter()
                    .any(|r| matches!(r, xai_chat_state::PersistenceRecord::Message(_)))
            );
            assert!(
                records.iter().any(|r| matches!(
                    r,
                    xai_chat_state::PersistenceRecord::AcknowledgedMessage(_)
                ))
            );
            assert!(
                records
                    .iter()
                    .any(|r| matches!(r, xai_chat_state::PersistenceRecord::ReplaceHistory(_)))
            );
            assert!(
                records
                    .iter()
                    .any(|r| matches!(r, xai_chat_state::PersistenceRecord::Flush))
            );

            drop(tee);
            wait_registry_gone(sid);
        });
    })
    .await
    .expect("tee methods path timed out");
}

// ── Chunk 2 write-back: capture-tee loop idempotency (HARD GATE) ──────
//
// Write-back re-enters capture as `replace_history`. These four tests prove
// the loop is idempotent for the write-back-shaped body specifically. If the
// tee shape is unclean, STOP — do not patch capture unilaterally.

fn view_src(id: &str) -> SessionThreadViewEntrySource {
    SessionThreadViewEntrySource {
        message_id: id.into(),
        idempotency_key: None,
    }
}

fn view_band(text: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
        content: text.into(),
        source_messages: Vec::new(),
    }))
}

fn view_user(text: &str, id: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
        content: text.into(),
        source_messages: vec![view_src(id)],
    }))
}

fn view_assistant_text(text: &str, id: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
        SessionAssistantMessage {
            content: vec![SessionAssistantPart {
                type_: SessionAssistantPartType::Text,
                text: Some(text.into()),
                thinking: None,
                thinking_signature: None,
                tool_call_id: None,
                tool_name: None,
                arguments: None,
            }],
            source_messages: vec![view_src(id)],

            provider: None,
            model: None,
            api: None,
        },
    ))
}

fn view_assistant_tool(name: &str, args: &str, id: &str) -> SessionThreadViewEntry {
    let arguments: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(args).unwrap_or_default();
    SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
        SessionAssistantMessage {
            content: vec![SessionAssistantPart {
                type_: SessionAssistantPartType::ToolCall,
                text: None,
                thinking: None,
                thinking_signature: None,
                tool_call_id: Some("c1".into()),
                tool_name: Some(name.into()),
                arguments: Some(arguments),
            }],
            source_messages: vec![view_src(id)],

            provider: None,
            model: None,
            api: None,
        },
    ))
}

fn view_tool_result(name: &str, content: &str, id: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(
        SessionToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: Some(name.into()),
            content: content.into(),
            is_error: None,
            source_messages: vec![view_src(id)],
        },
    ))
}

/// Realistic post-compact typed session view (bands = empty sources).
fn realistic_post_compact_view() -> SessionThreadView {
    SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            view_band("[context · brief]\nSession goals and constraints."),
            view_band("[context · detailed]\nTurn-by-turn compressed history."),
            view_band("[context · smooth]\nNarrative bridge into the live tail."),
            view_user("please investigate area 9", "u1"),
            view_assistant_tool("bash", r#"{"cmd":"ls"}"#, "a1"),
            view_tool_result("bash", "file_a\nfile_b", "tr1"),
            view_user("[runtime note] cwd switched", "rn1"),
            SessionThreadViewEntry::Runtime(SessionThreadViewRuntimeEntry::ModelChange(
                SessionModelChangeEntry {
                    provider: "xai".into(),
                    model_id: "grok-4".into(),
                    source_messages: vec![view_src("mc1")],
                },
            )),
            view_user("live-2", "u2"),
            view_assistant_text("a2", "a2"),
        ],
    }
}

fn realistic_kinds(view: &SessionThreadView) -> SourceKindIndex {
    SourceKindIndex::assume_sourced_users_are_prompts(view)
        .with("rn1", lhc::messages::MessageKind::RuntimeNote)
}

fn writeback_fixture() -> (Vec<ConversationItem>, Vec<ConversationItem>) {
    let mut u0 = ConversationItem::user("old-0");
    u0.set_prompt_index(0);
    let mut u1 = ConversationItem::user("please investigate area 9");
    u1.set_prompt_index(1);
    let mut u2 = ConversationItem::user("live-2");
    u2.set_prompt_index(2);
    // Causally coherent with the LHC view's `[tool call · bash]` /
    // `[tool result · bash]` — native assistant must name the tool.
    let tool_asst = ConversationItem::assistant_tool_calls(vec![ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: "{\"cmd\":\"ls\"}".into(),
    }]);
    let native = vec![
        ConversationItem::system("sys"),
        u0,
        ConversationItem::assistant("a0"),
        u1,
        tool_asst,
        ConversationItem::tool_result("c1", "file_a\nfile_b"),
        ConversationItem::assistant("done looking"),
        u2,
        ConversationItem::assistant("a2"),
    ];
    let view = realistic_post_compact_view();
    let kinds = realistic_kinds(&view);
    let body = build_writeback_conversation(&native, &view, &kinds).expect("writeback body");
    (native, body)
}

const WB_BAND_SUMMARY_NEEDLE: &str = "[context · brief]";

/// Gate property: write-back twice with the same ctx → byte-identical body + keys.
#[test]
fn writeback_body_is_fixpoint_through_replace_history() {
    let root = TempDir::new().unwrap();
    let sid = "cert-wb-fixpoint";
    let (native, body1) = writeback_fixture();
    let view = realistic_post_compact_view();
    let kinds = realistic_kinds(&view);
    let body2 = build_writeback_conversation(&body1, &view, &kinds).expect("second writeback");
    let fp = |items: &[ConversationItem]| {
        items
            .iter()
            .map(|i| {
                let kind = match i {
                    ConversationItem::System(_) => "system",
                    ConversationItem::User(u) if u.synthetic_reason.is_some() => "user_meta",
                    ConversationItem::User(_) => "user",
                    ConversationItem::Assistant(a) if !a.tool_calls.is_empty() => "assistant_tools",
                    ConversationItem::Assistant(_) => "assistant",
                    ConversationItem::ToolResult(_) => "tool_result",
                    ConversationItem::Reasoning(_) => "reasoning",
                    ConversationItem::BackendToolCall(_) => "backend_tool_call",
                };
                let tools = match i {
                    ConversationItem::Assistant(a) => a
                        .tool_calls
                        .iter()
                        .map(|t| format!("{}:{}", t.id, t.name))
                        .collect::<Vec<_>>()
                        .join(","),
                    ConversationItem::ToolResult(tr) => tr.tool_call_id.clone(),
                    _ => String::new(),
                };
                format!(
                    "{}:{:?}:{}:{}",
                    kind,
                    match i {
                        ConversationItem::User(u) => u.prompt_index,
                        _ => None,
                    },
                    tools,
                    i.text_content()
                )
            })
            .collect::<Vec<_>>()
            .join("||")
    };
    assert_eq!(
        fp(&body1),
        fp(&body2),
        "build_writeback_conversation must be a fixpoint"
    );
    assert_eq!(native_prompt_indices(&body1), vec![1, 2]);

    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let _ = wait_events(&handle, 1);
    handle.replace_history(&body1);
    handle.flush_blocking();
    let once = wait_events(&handle, 1);
    let once_keys = keys(&once);
    handle.replace_history(&body2);
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let again = handle.list_events_blocking().unwrap();
    assert_eq!(
        keys(&again),
        once_keys,
        "fixpoint body must not re-key on second replace"
    );
    handle.shutdown_blocking();
}

/// (1) Prune-shaped replace of a post-write-back body emits nothing.
/// Would fail if prune-shaped replace started minting new keys for survivors.
#[test]
fn writeback_prune_shaped_replace_emits_nothing() {
    let root = TempDir::new().unwrap();
    let sid = "cert-wb-prune";
    let (native, body) = writeback_fixture();
    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let _seeded = wait_events(&handle, 1);
    handle.replace_history(&body);
    handle.flush_blocking();
    let after_wb = wait_events(&handle, 1);
    let before_keys = keys(&after_wb);
    let before_len = after_wb.len();

    // Prune-shaped: drop band meta + earlier turns; keep system + live tail.
    let pruned: Vec<_> = body
        .iter()
        .filter(|i| match i {
            ConversationItem::User(u) if u.synthetic_reason.is_none() => {
                i.text_content().contains("live-2")
            }
            ConversationItem::Assistant(_) => i.text_content().contains("a2"),
            ConversationItem::System(_) => true,
            _ => false,
        })
        .cloned()
        .collect();
    assert!(
        pruned.len() < body.len(),
        "fixture must actually prune ({} vs {})",
        pruned.len(),
        body.len()
    );
    for _ in 0..3 {
        handle.replace_history(&pruned);
    }
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let after = handle.list_events_blocking().unwrap();
    assert_eq!(
        after.len(),
        before_len,
        "prune-shaped write-back path must add zero events"
    );
    assert_eq!(keys(&after), before_keys);
    handle.shutdown_blocking();
}

/// (2) Genuine compact write-back records the band summary exactly once.
/// Would fail on zero (summary lost) or twice (tee double-fire / re-key).
#[test]
fn writeback_genuine_compact_summary_records_exactly_once() {
    let root = TempDir::new().unwrap();
    let sid = "cert-wb-summary-once";
    let (native, body) = writeback_fixture();
    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let before = wait_events(&handle, 1);
    let before_keys = keys(&before);

    handle.replace_history(&body);
    handle.flush_blocking();
    let after = wait_events(&handle, before.len() + 1);
    let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
    assert!(
        !new_keys.is_empty(),
        "genuine write-back must record something"
    );
    let summary_hits: Vec<_> = after
        .iter()
        .filter(|e| {
            e.text_payload()
                .is_some_and(|p| p.text.contains(WB_BAND_SUMMARY_NEEDLE))
        })
        .collect();
    assert_eq!(
        summary_hits.len(),
        1,
        "band summary must appear exactly once, got {}",
        summary_hits.len()
    );
    assert!(
        new_keys.contains(summary_hits[0].idempotency_key()),
        "summary must be among the newly recorded keys"
    );
    handle.shutdown_blocking();
}

/// (3) Repeated write-backs of an unchanged body record nothing.
/// Would fail if survivors were re-keyed on each replace.
#[test]
fn writeback_repeated_unchanged_body_records_nothing() {
    let root = TempDir::new().unwrap();
    let sid = "cert-wb-repeat";
    let (native, body) = writeback_fixture();
    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let _ = wait_events(&handle, 1);
    handle.replace_history(&body);
    handle.flush_blocking();
    let once = wait_events(&handle, 1);
    let once_keys = keys(&once);
    let once_len = once.len();

    for _ in 0..4 {
        handle.replace_history(&body);
    }
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let again = handle.list_events_blocking().unwrap();
    assert_eq!(again.len(), once_len);
    assert_eq!(keys(&again), once_keys);
    handle.shutdown_blocking();
}

/// (4) Crash mid-write-back must not double-record on retry.
/// Arms a crash after **one novel (Recorded)** event of the replace is
/// committed — not after the preserved system message (which is dedup-skipped).
/// Would fail if a partial apply + retry minted a second summary key.
#[test]
fn writeback_crash_mid_replace_no_double_on_retry() {
    let root = TempDir::new().unwrap();
    let sid = "cert-wb-crash";
    let (native, body) = writeback_fixture();
    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let seeded = wait_events(&handle, 1);
    let seeded_keys = keys(&seeded);
    let seeded_len = seeded.len();

    // Partial apply: crash after the first *novel* event (band summary) lands.
    handle.arm_crash_mid_replace(1);
    handle.replace_history(&body);
    // Worker exits on mid-replace crash; wait for registry clear.
    wait_registry_gone(sid);
    thread::sleep(Duration::from_millis(200));

    // Discriminating observation: mid-apply crash must have committed the
    // novel band summary (not merely died before any new Recorded event).
    let probe = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    thread::sleep(Duration::from_millis(100));
    let partial = probe.list_events_blocking().unwrap();
    let summary_count = |ev: &[EventRecord]| {
        ev.iter()
            .filter(|e| {
                e.text_payload()
                    .is_some_and(|p| p.text.contains(WB_BAND_SUMMARY_NEEDLE))
            })
            .count()
    };
    assert_eq!(
        summary_count(&partial),
        1,
        "mid-apply crash must leave exactly one novel band summary committed \
         (arming after the dedup-skipped system message is a vacuous no-op)"
    );
    assert!(
        partial.len() > seeded_len,
        "partial apply must grow past bootstrap seed"
    );
    probe.shutdown_blocking();
    wait_registry_gone(sid);

    // Host already holds the write-back body; reopen and apply again.
    let handle2 = spawn_capture(sid, Some("/tmp"), &body, Some(root.path()), None).unwrap();
    handle2.flush_blocking();
    let mut after_retry = wait_events(&handle2, seeded_len);
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(50));
        let cur = handle2.list_events_blocking().unwrap();
        if cur.len() == after_retry.len() && cur.len() > seeded_len {
            after_retry = cur;
            break;
        }
        after_retry = cur;
    }
    assert_eq!(
        summary_count(&after_retry),
        1,
        "after crash+retry summary must appear exactly once"
    );
    let keys_once = keys(&after_retry);
    assert!(seeded_keys.is_subset(&keys_once));

    handle2.replace_history(&body);
    handle2.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let again = handle2.list_events_blocking().unwrap();
    assert_eq!(
        summary_count(&again),
        1,
        "second write-back must not double summary"
    );
    assert_eq!(keys(&again), keys_once);
    handle2.shutdown_blocking();
}

/// Q4 — live-tail kinds round-trip: `runtime_note`, `user_prompt`,
/// `tool_call`, `tool_result`, `assistant_text` keep their event kinds after
/// write-back. Would fail if the translator flattened tools/notes to the
/// wrong kind.
#[test]
fn writeback_live_tail_kinds_round_trip() {
    let root = TempDir::new().unwrap();
    let sid = "cert-wb-tail-kinds";
    const NOTE: &str = "task-completed synthetic wake";
    let mut real = ConversationItem::user("real-prompt");
    real.set_prompt_index(0);
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user_meta(NOTE),
        real,
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{\"cmd\":\"ls\"}".into(),
        }]),
        ConversationItem::tool_result("c1", "file_a"),
        ConversationItem::assistant("done"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let seeded = wait_events(&handle, 5);
    let kind_count = |ev: &[EventRecord], kind: &str| {
        ev.iter()
            .filter(|e| e.event_kind().as_str() == kind)
            .count()
    };
    assert!(
        kind_count(&seeded, "runtime_note") >= 1,
        "bootstrap records runtime_note"
    );
    assert!(
        kind_count(&seeded, "user_prompt") >= 1,
        "bootstrap records user_prompt"
    );
    assert!(
        kind_count(&seeded, "tool_call") >= 1,
        "bootstrap records tool_call"
    );
    assert!(
        kind_count(&seeded, "tool_result") >= 1,
        "bootstrap records tool_result"
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (view, kinds) = rt
        .block_on(handle.get_classify_context())
        .expect("classify context");
    let body = build_writeback_conversation(&native, &view, &kinds).expect("writeback");
    assert!(
        body.iter()
            .any(|i| matches!(i, ConversationItem::ToolResult(_))),
        "write-back body must conserve ToolResult"
    );
    assert!(
        body.iter().any(|i| matches!(
            i,
            ConversationItem::Assistant(a) if !a.tool_calls.is_empty()
        )),
        "write-back body must conserve assistant tool_calls"
    );
    assert!(
        body.iter().any(|i| {
            matches!(i, ConversationItem::User(u) if u.synthetic_reason.is_some())
                && i.text_content().contains(NOTE)
        }),
        "runtime note must remain user_meta"
    );
    assert_eq!(native_prompt_indices(&body), vec![0]);

    handle.replace_history(&body);
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let after = handle.list_events_blocking().unwrap();
    let note_as_prompt = after.iter().any(|e| {
        e.event_kind().as_str() == "user_prompt"
            && e.text_payload().is_some_and(|p| p.text.contains(NOTE))
    });
    assert!(
        !note_as_prompt,
        "write-back must not promote runtime_note → user_prompt"
    );
    assert!(
        kind_count(&after, "tool_call") >= 1,
        "tool_call must survive write-back"
    );
    assert!(
        kind_count(&after, "tool_result") >= 1,
        "tool_result must survive write-back"
    );
    assert!(
        kind_count(&after, "user_prompt") >= 1,
        "user_prompt must survive write-back"
    );
    handle.shutdown_blocking();
}

/// Q1 — whole-index failure aborts serve (Native) and write-back (Err).
/// Per-entry unknown still classifies synthetic (separate test in serving).
#[test]
fn classify_whole_index_failure_fails_open_no_substitution_no_writeback() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-q1-index-fail";
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("hello"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let _ = wait_events(&handle, 1);

    set_force_classify_list_failure(true);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let decision = rt.block_on(serve_request_context(sid, &native, None));
    set_compact_mode_for_test(Some(CompactMode::Replace));
    let wb = rt.block_on(grok_lhc_host::replace_compact_for_writeback(sid));
    set_compact_mode_for_test(None);
    set_force_classify_list_failure(false);
    assert!(
        matches!(
            decision,
            ServeDecision::Native {
                reason: "get_classify_context_failed"
            }
        ),
        "whole-index failure must fail open to Native, got {decision:?}"
    );
    assert!(
        wb.is_err(),
        "whole-index failure must abort write-back, got {wb:?}"
    );

    handle.shutdown_blocking();
}

/// H6 (adapter-reachable): production [`replace_compact_for_writeback`] is the
/// compact+view fetch path. Deleting / no-op'ing it fails this test (N2).
#[test]
fn writeback_replace_compact_for_writeback_is_production_path() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-wb-replace-path";
    let (native, _) = writeback_fixture();
    let sampler = Arc::new(MockLhcInferenceSampler::new());
    let handle =
        spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), Some(sampler)).unwrap();
    let _ = wait_events(&handle, 1);

    reset_compact_call_counters();
    set_compact_mode_for_test(Some(CompactMode::Replace));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let writeback = rt
        .block_on(grok_lhc_host::replace_compact_for_writeback(sid))
        .expect("replace_compact_for_writeback must succeed under Replace mode");
    set_compact_mode_for_test(None);

    assert_eq!(
        replace_call_count(),
        1,
        "production replace_compact_for_writeback must count the compact attempt"
    );
    assert!(
        !writeback.view.entries.is_empty(),
        "production path must return a post-compact session view"
    );
    // Prove the view is usable by the shared write-back translator.
    let _body = build_writeback_conversation(&native, &writeback.view, &writeback.kinds)
        .expect("session view from production path must translate");
    handle.shutdown_blocking();
}

/// N1 — Background mode drains PromptSmoothing between turns; compact is
/// selection-only. ToolResultSummary sampler ops remain absent (**DERIV-12** /
/// port `FORCE_TOOL_RESULT_SUMMARY_FALLBACK`).
#[test]
fn n1_background_drain_prompt_smoothing_before_compact_tool_result_absent_by_deriv12() {
    use std::time::Instant;

    use grok_lhc_host::LhcInferenceOp;
    use xai_grok_sampling_types::ToolCall;

    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-n1-bg-drain-lanes";
    let blob = "word ".repeat(40);
    let large_tool = "line\n".repeat(3_000);
    let mut native = vec![ConversationItem::system("sys")];
    for t in 0..3 {
        let mut u = ConversationItem::user(format!("turn {t} {blob}"));
        u.set_prompt_index(t);
        native.push(u);
        if t == 1 {
            native.push(ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }]));
            native.push(ConversationItem::tool_result("c1", large_tool.clone()));
        }
        native.push(ConversationItem::assistant(format!("answer {t} {blob}")));
    }

    let counter = Arc::new(CountingLhcInferenceSampler::new());
    let handle = spawn_capture(
        sid,
        Some("/tmp"),
        &native,
        Some(root.path()),
        Some(counter.clone()),
    )
    .unwrap();
    let _ = wait_events(&handle, 1);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Production path: background scheduler settles between turns — host does
    // not drain inside compact.
    rt.block_on(handle.drain_settled())
        .expect("background drainSettled");

    let ops_before = counter.call_ops();
    eprintln!("N1 ops after background settle (before compact): {ops_before:?}");
    assert!(
        ops_before.contains(&LhcInferenceOp::SmoothPrompt),
        "Background mode must run PromptSmoothing before compact; got {ops_before:?}"
    );
    assert!(
        !ops_before.contains(&LhcInferenceOp::SummarizeToolResult),
        "ToolResultSummary sampler ops must be absent (DERIV-12 / \
         FORCE_TOOL_RESULT_SUMMARY_FALLBACK). Got {ops_before:?}"
    );

    set_compact_mode_for_test(Some(CompactMode::Replace));
    let t0 = Instant::now();
    let wb = rt
        .block_on(grok_lhc_host::replace_compact_for_writeback(sid))
        .expect("replace_compact_for_writeback");
    let compact_wall = t0.elapsed();
    set_compact_mode_for_test(None);

    let ops_after = counter.call_ops();
    eprintln!(
        "N1 compact wall={compact_wall:?}; ops after compact: {ops_after:?} \
         (compact must not bill a 400s drain)"
    );
    assert!(
        compact_wall < Duration::from_secs(5),
        "compact must be selection-walk fast after background drain; took {compact_wall:?}"
    );
    assert_eq!(
        ops_after.len(),
        ops_before.len(),
        "compact must not start new derivation sampler ops; before={ops_before:?} after={ops_after:?}"
    );
    assert!(
        !wb.view.entries.is_empty(),
        "production path returns a view"
    );
    handle.shutdown_blocking();
}

/// N2 — `/lhc off` unregisters immediately; close settles in-flight work under
/// [`grok_lhc_host::DRAIN_SETTLED_AT_CLOSE`] (not an unbounded host drain).
#[test]
fn n2_shutdown_unregisters_immediately_close_settle_capped() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use grok_lhc_host::{
        DRAIN_SETTLED_AT_CLOSE, LhcInferenceError, LhcInferenceErrorKind, LhcInferenceFuture,
        LhcInferenceRequest, LhcInferenceSample, LhcInferenceSampler, capture_active,
    };
    use tokio_util::sync::CancellationToken;

    struct SleepySampler {
        started: Arc<AtomicUsize>,
    }
    impl LhcInferenceSampler for SleepySampler {
        fn sample(
            &self,
            req: LhcInferenceRequest,
            cancel: CancellationToken,
        ) -> LhcInferenceFuture {
            let started = Arc::clone(&self.started);
            let label = req.op().as_prompt_label().to_string();
            let max_output_tokens = req.max_output_tokens();
            Box::pin(async move {
                started.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    _ = cancel.cancelled() => Err(LhcInferenceError {
                        kind: LhcInferenceErrorKind::Cancelled,
                        detail: "sleepy cancelled".into(),
                        request_messages: None,
                    }),
                    _ = tokio::time::sleep(Duration::from_secs(60)) => Ok(LhcInferenceSample {
                        text: "sleepy".into(),
                        model: "sleepy".into(),
                        prompt_label: label,
                        request_messages: vec![],
                        max_output_tokens,
                    }),
                }
            })
        }
    }

    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-n2-shutdown-cap";
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("u0 word word"),
        ConversationItem::assistant("a0 word word"),
    ];
    let started = Arc::new(AtomicUsize::new(0));
    let sampler = Arc::new(SleepySampler {
        started: Arc::clone(&started),
    });
    let handle =
        spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), Some(sampler)).unwrap();
    let _ = wait_events(&handle, 1);
    assert!(capture_active(sid));

    // Give background mode a moment to start the sleepy sample (correct).
    let start_deadline = Instant::now() + Duration::from_secs(5);
    while started.load(Ordering::SeqCst) == 0 && Instant::now() < start_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    let t0 = Instant::now();
    // Product path: unregister is immediate; worker close is capped.
    shutdown_session(sid);
    assert!(
        !capture_active(sid),
        "N2: capture_active must be false immediately after shutdown_session"
    );
    handle.shutdown_blocking();
    let elapsed = t0.elapsed();
    let cap_plus = DRAIN_SETTLED_AT_CLOSE + Duration::from_secs(5);
    assert!(
        elapsed < cap_plus,
        "N2: close settle must respect DRAIN_SETTLED_AT_CLOSE \
         ({DRAIN_SETTLED_AT_CLOSE:?}); took {elapsed:?} (unbounded would be ~60s)"
    );
    eprintln!(
        "N2: unregister immediate; shutdown_blocking in {elapsed:?} \
         (cap={DRAIN_SETTLED_AT_CLOSE:?}); samples_started={}",
        started.load(Ordering::SeqCst)
    );

    let handle2 = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    assert!(capture_active(sid));
    handle2.shutdown_blocking();
}

/// R3 (re-pointed) — after turn abort during compact: **no snapshot install**,
/// native/view unchanged. Background derivation continuing is fine (not a leak).
/// Call-cessation / TokenWatchSampler is no longer the invariant.
#[test]
fn r3_abort_installs_no_compact_snapshot_derivation_may_continue() {
    use grok_lhc_host::{
        CompactDrainOutcome, CountingLhcInferenceSampler, last_compact_drain_outcome,
        replace_compact_for_writeback_with_cancel_signal, set_compact_params_override_for_test,
        set_sever_compact_signal_for_test, set_use_deterministic_inference_for_test,
    };
    use lhc::shared_tech::view::{PartialViewProfilePercentages, ViewCompactParams};
    use lhc::thread_view::CompactAbortSignal;
    use lhc::thread_view::internal::seam::{ViewInjectionPoint, set_view_injection_hook};
    use tokio_util::sync::CancellationToken;

    fn tight_params() -> ViewCompactParams {
        ViewCompactParams {
            lower_bound: Some(400.0),
            percentages: Some(PartialViewProfilePercentages {
                full: Some(30.0),
                smooth: Some(25.0),
                detailed: Some(20.0),
                brief: Some(25.0),
            }),
        }
    }

    fn context_fingerprint(handle: &CaptureHandle, rt: &tokio::runtime::Runtime) -> String {
        let ctx = rt
            .block_on(handle.get_llm_request_context())
            .expect("llm context");
        serde_json::to_string(&ctx).unwrap_or_default()
    }

    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let blob = "word ".repeat(80);
    let mut native = vec![ConversationItem::system("sys")];
    for t in 0..8 {
        let mut u = ConversationItem::user(format!("turn {t} {blob}"));
        u.set_prompt_index(t);
        native.push(u);
        native.push(ConversationItem::assistant(format!("answer {t} {blob}")));
    }

    let sid = "cert-r3-no-install";
    let counter = Arc::new(CountingLhcInferenceSampler::new());
    let handle = spawn_capture(
        sid,
        Some("/tmp"),
        &native,
        Some(root.path()),
        Some(counter.clone()),
    )
    .unwrap();
    let _ = wait_events(&handle, 1);
    set_compact_mode_for_test(Some(CompactMode::Replace));
    set_use_deterministic_inference_for_test(true);
    set_sever_compact_signal_for_test(false);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _ = rt.block_on(handle.drain_settled());
    let ops_at_abort_gate = counter.call_ops().len();
    let prior = context_fingerprint(&handle, &rt);

    let cancel = CancellationToken::new();
    let signal = CompactAbortSignal::new();
    let cancel_h = cancel.clone();
    let signal_h = signal.clone();
    set_view_injection_hook(
        ViewInjectionPoint::CompactWrite,
        Some(Arc::new(move || {
            // Drive the live CompactAbortSignal atomic directly — no OS bridge
            // thread. tokio::select! cannot preempt sync compact compute.
            cancel_h.cancel();
            signal_h.abort();
            assert!(
                signal_h.aborted(),
                "R3: CompactAbortSignal atomic must be aborted before \
                 compact_stopped re-read"
            );
        })),
    );
    set_compact_params_override_for_test(Some(tight_params()));
    let result = rt.block_on(replace_compact_for_writeback_with_cancel_signal(
        sid,
        cancel,
        signal.clone(),
    ));
    set_view_injection_hook(ViewInjectionPoint::CompactWrite, None);

    assert!(
        matches!(&result, Err(e) if e == "compact_cancelled"),
        "R3: compact must cancel at snapshot checkpoint; got {result:?}"
    );
    assert!(signal.aborted(), "R3: signal must be aborted");
    let after = context_fingerprint(&handle, &rt);
    assert_eq!(
        after, prior,
        "R3: LLM request context unchanged — no snapshot install after abort"
    );
    assert_eq!(
        last_compact_drain_outcome(sid),
        Some(CompactDrainOutcome::AbandonedByCancel),
        "R3: status must surface abandon"
    );
    // Background derivation may continue — do not require call cessation.
    let ops_after = counter.call_ops().len();
    eprintln!(
        "R3: no install; ops_at_gate={ops_at_abort_gate} ops_after={ops_after} \
         (continuation is fine)"
    );

    set_use_deterministic_inference_for_test(false);
    set_compact_mode_for_test(None);
    handle.shutdown_blocking();
    wait_registry_gone(sid);
}

/// R1 — port `CompactAbortSignal` must observe turn abort during sync compute
/// so the snapshot write does not land. `tokio::select!` cannot preempt.
#[test]
fn r1_cancel_at_snapshot_write_leaves_view_unchanged() {
    use grok_lhc_host::{
        CountingLhcInferenceSampler, replace_compact_for_writeback_with_cancel_signal,
        set_compact_params_override_for_test, set_sever_compact_signal_for_test,
        set_use_deterministic_inference_for_test,
    };
    use lhc::shared_tech::view::{PartialViewProfilePercentages, ViewCompactParams};
    use lhc::thread_view::CompactAbortSignal;
    use lhc::thread_view::internal::seam::{ViewInjectionPoint, set_view_injection_hook};
    use tokio_util::sync::CancellationToken;

    fn tight_params() -> ViewCompactParams {
        ViewCompactParams {
            lower_bound: Some(400.0),
            percentages: Some(PartialViewProfilePercentages {
                full: Some(30.0),
                smooth: Some(25.0),
                detailed: Some(20.0),
                brief: Some(25.0),
            }),
        }
    }

    fn context_fingerprint(handle: &CaptureHandle, rt: &tokio::runtime::Runtime) -> String {
        let ctx = rt
            .block_on(handle.get_llm_request_context())
            .expect("llm context");
        serde_json::to_string(&ctx).unwrap_or_default()
    }

    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let blob = "word ".repeat(80);
    let mut native = vec![ConversationItem::system("sys")];
    for t in 0..8 {
        let mut u = ConversationItem::user(format!("turn {t} {blob}"));
        u.set_prompt_index(t);
        native.push(u);
        native.push(ConversationItem::assistant(format!("answer {t} {blob}")));
    }

    // --- RESTORED path: signal wired → abort at CompactWrite → no snapshot ---
    let sid = "cert-r1-signal-wired";
    let handle = spawn_capture(
        sid,
        Some("/tmp"),
        &native,
        Some(root.path()),
        Some(Arc::new(CountingLhcInferenceSampler::new())),
    )
    .unwrap();
    let _ = wait_events(&handle, 1);
    set_compact_mode_for_test(Some(CompactMode::Replace));
    set_use_deterministic_inference_for_test(true);
    set_sever_compact_signal_for_test(false);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let prior = context_fingerprint(&handle, &rt);

    let cancel = CancellationToken::new();
    let signal = CompactAbortSignal::new();
    let cancel_h = cancel.clone();
    let signal_h = signal.clone();
    set_view_injection_hook(
        ViewInjectionPoint::CompactWrite,
        Some(Arc::new(move || {
            // Drive the live CompactAbortSignal atomic directly — no OS bridge.
            cancel_h.cancel();
            signal_h.abort();
            assert!(
                signal_h.aborted(),
                "R1: CompactAbortSignal atomic must be aborted before \
                 compact_stopped re-read"
            );
        })),
    );
    set_compact_params_override_for_test(Some(tight_params()));
    let result = rt.block_on(replace_compact_for_writeback_with_cancel_signal(
        sid,
        cancel,
        signal.clone(),
    ));
    set_view_injection_hook(ViewInjectionPoint::CompactWrite, None);
    assert!(
        matches!(&result, Err(e) if e == "compact_cancelled"),
        "R1 restored: compact must cancel at snapshot checkpoint; got {result:?}"
    );
    assert!(signal.aborted(), "R1: signal must be aborted");
    let after = context_fingerprint(&handle, &rt);
    assert_eq!(
        after, prior,
        "R1 restored: LLM request context must be unchanged (no snapshot write)"
    );
    eprintln!("R1 RESTORED: compact_cancelled + view unchanged");
    set_use_deterministic_inference_for_test(false);
    set_compact_mode_for_test(None);
    handle.shutdown_blocking();
    wait_registry_gone(sid);

    // --- SEVERED path: signal: None → CompactWrite cancel does not stop write ---
    let sid2 = "cert-r1-signal-severed";
    let handle2 = spawn_capture(
        sid2,
        Some("/tmp"),
        &native,
        Some(root.path()),
        Some(Arc::new(CountingLhcInferenceSampler::new())),
    )
    .unwrap();
    let _ = wait_events(&handle2, 1);
    set_compact_mode_for_test(Some(CompactMode::Replace));
    set_use_deterministic_inference_for_test(true);
    set_sever_compact_signal_for_test(true);

    let prior2 = context_fingerprint(&handle2, &rt);
    let cancel2 = CancellationToken::new();
    let signal2 = CompactAbortSignal::new();
    let cancel2_h = cancel2.clone();
    set_view_injection_hook(
        ViewInjectionPoint::CompactWrite,
        Some(Arc::new(move || {
            cancel2_h.cancel();
            // Severed path: signal: None in CompactOpts — aborting a local
            // signal (or cancelling the token) must not stop the write.
            std::thread::sleep(Duration::from_millis(50));
        })),
    );
    set_compact_params_override_for_test(Some(tight_params()));
    let severed = rt.block_on(replace_compact_for_writeback_with_cancel_signal(
        sid2, cancel2, signal2,
    ));
    set_view_injection_hook(ViewInjectionPoint::CompactWrite, None);
    set_sever_compact_signal_for_test(false);
    let after2 = context_fingerprint(&handle2, &rt);
    let wrote = after2 != prior2 || severed.is_ok();
    eprintln!(
        "R1 SEVERED: result={severed:?} view_changed={} (expect write landed)",
        after2 != prior2
    );
    assert!(
        wrote,
        "R1 break-watch: with signal: None, CompactWrite cancel must not block \
         the snapshot — otherwise the test is not proving the port checkpoint"
    );
    set_use_deterministic_inference_for_test(false);
    set_compact_mode_for_test(None);
    handle2.shutdown_blocking();
    wait_registry_gone(sid2);
}

/// Count lingering `lhc-compact-abort-bridge` threads (Linux `/proc`).
///
/// Kernel `comm` is truncated to 15 chars → `lhc-compact-abo`.
fn count_compact_abort_bridge_threads() -> usize {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    dir.filter_map(|e| e.ok())
        .filter(|e| {
            std::fs::read_to_string(e.path().join("comm"))
                .map(|n| {
                    let n = n.trim();
                    n.starts_with("lhc-compact-ab") || n == "lhc-compact-abort-bridge"
                })
                .unwrap_or(false)
        })
        .count()
}

/// T1 — successful compact must not leak an OS cancel→signal bridge thread.
///
/// Production drives [`lhc::thread_view::CompactAbortSignal`]'s live atomic
/// from the DropGuard (no bridge). N successful compacts must leave the
/// bridge-thread count at baseline.
#[test]
fn t1_successful_compact_leaves_no_abort_bridge_threads() {
    use grok_lhc_host::{
        CountingLhcInferenceSampler, replace_compact_for_writeback,
        set_compact_params_override_for_test, set_use_deterministic_inference_for_test,
    };
    use lhc::shared_tech::view::{PartialViewProfilePercentages, ViewCompactParams};

    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let blob = "word ".repeat(80);
    let mut native = vec![ConversationItem::system("sys")];
    for t in 0..6 {
        let mut u = ConversationItem::user(format!("turn {t} {blob}"));
        u.set_prompt_index(t);
        native.push(u);
        native.push(ConversationItem::assistant(format!("answer {t} {blob}")));
    }

    let sid = "cert-t1-bridge-leak";
    let handle = spawn_capture(
        sid,
        Some("/tmp"),
        &native,
        Some(root.path()),
        Some(Arc::new(CountingLhcInferenceSampler::new())),
    )
    .unwrap();
    let _ = wait_events(&handle, 1);
    set_compact_mode_for_test(Some(CompactMode::Replace));
    set_use_deterministic_inference_for_test(true);
    set_compact_params_override_for_test(Some(ViewCompactParams {
        lower_bound: Some(400.0),
        percentages: Some(PartialViewProfilePercentages {
            full: Some(30.0),
            smooth: Some(25.0),
            detailed: Some(20.0),
            brief: Some(25.0),
        }),
    }));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let before = count_compact_abort_bridge_threads();
    const N: usize = 3;
    for i in 0..N {
        set_compact_params_override_for_test(Some(ViewCompactParams {
            lower_bound: Some(400.0),
            percentages: Some(PartialViewProfilePercentages {
                full: Some(30.0),
                smooth: Some(25.0),
                detailed: Some(20.0),
                brief: Some(25.0),
            }),
        }));
        let wb = rt
            .block_on(replace_compact_for_writeback(sid))
            .unwrap_or_else(|e| panic!("compact {i} failed: {e}"));
        assert!(wb.receipt_total_tokens > 0, "compact {i} empty receipt");
    }
    // Join windows: worker must finish; give any leaked bridge a chance to show.
    std::thread::sleep(Duration::from_millis(100));
    let after = count_compact_abort_bridge_threads();
    eprintln!("VERIFIER bridge threads before={before} after={after} (N={N} successful compacts)");
    assert_eq!(
        after, before,
        "T1: {N} successful replace_compact_for_writeback calls leaked \
         lhc-compact-abort-bridge threads (before={before} after={after}). \
         Success path must not leave a cancel waiter alive."
    );

    set_use_deterministic_inference_for_test(false);
    set_compact_mode_for_test(None);
    handle.shutdown_blocking();
    wait_registry_gone(sid);
}

/// Drain architecture repair — t3code-shaped certification measurements.
///
/// 1. ready ≈ total at compact threshold (after between-turn settle)
/// 2. compact wall-time in fractions of a second (not hundreds)
/// 3. queue settles between turns
/// 4. first-touch catch-up absorbs a pre-existing backlog at open
#[test]
fn drain_architecture_background_mode_cert_measurements() {
    use std::time::Instant;

    use grok_lhc_host::{
        CountingLhcInferenceSampler, set_compact_params_override_for_test,
        set_use_deterministic_inference_for_test,
    };
    use lhc::create_deterministic_inference_callbacks;
    use lhc::init_lhc;
    use lhc::intake_stream::MessageEventInput;
    use lhc::shared_tech::derivation::{SdkConfig, SdkMode};
    use lhc::shared_tech::errors::OpResult;
    use lhc::shared_tech::view::{PartialViewProfilePercentages, ViewCompactParams};
    use lhc::threads::{NewThreadInput, ThreadRef};
    use serde_json::{Map, json};

    fn ev(kind: &str, payload: Map<String, serde_json::Value>, key: &str) -> MessageEventInput {
        MessageEventInput {
            event_kind: kind.into(),
            idempotency_key: Some(key.into()),
            actor: "grok".into(),
            harness: "drain-arch".into(),
            payload,
            extra: Map::new(),
        }
    }

    fn health_ready_total(h: &lhc::shared_tech::inspect::HealthReport) -> (i64, i64) {
        let mut ready = 0i64;
        let mut total = 0i64;
        for o in &h.owners {
            ready += o.counts.ready;
            total += o.counts.ready + o.counts.pending + o.counts.failed + o.counts.blocked;
        }
        (ready, total)
    }

    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-drain-arch-bg";

    // --- (4) First-touch: seed a Manual backlog, then open Background ---
    let backlog_path = root.path().join("threads");
    std::fs::create_dir_all(&backlog_path).unwrap();
    let thread_path = thread_file_path(root.path(), sid);
    let registry = root.path().join("registry.sqlite");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let manual = init_lhc(SdkConfig {
            inference_callbacks: Some(create_deterministic_inference_callbacks()),
            inference: None,
            mode: SdkMode::Manual,
            clock: None,
            guards: None,
            tool_result: None,
            lease: None,
            chunk_policy: None,
            view: None,
        });
        let created = manual
            .threads
            .new_thread(NewThreadInput {
                file_path: thread_path.to_string_lossy().into_owned(),
                title: Some("drain-arch".into()),
                cwd: None,
                registry_path: Some(registry.to_string_lossy().into_owned()),
            })
            .await;
        let OpResult::Ok { value: info } = created else {
            panic!("new_thread failed");
        };
        let ref_ = ThreadRef::file_path(info.file_path);
        let blob = "word ".repeat(30);
        let mut batch = Vec::new();
        for t in 0..4 {
            batch.push(ev(
                "user_prompt",
                json!({ "text": format!("turn {t} {blob}") })
                    .as_object()
                    .cloned()
                    .unwrap(),
                &format!("u{t}"),
            ));
            batch.push(ev(
                "assistant_text",
                json!({ "text": format!("answer {t} {blob}") })
                    .as_object()
                    .cloned()
                    .unwrap(),
                &format!("a{t}"),
            ));
            batch.push(ev("turn_end", Map::new(), &format!("te{t}")));
        }
        let submitted = manual
            .intake_stream
            .message_events(ref_.clone(), &batch)
            .await;
        assert!(matches!(submitted, OpResult::Ok { .. }), "{submitted:?}");
        // Deliberately do NOT drain — leave backlog for first-touch.
        drop(manual);
    });

    let counter = Arc::new(CountingLhcInferenceSampler::new());
    // Empty bootstrap: identity open is touch-suppressed (RECORD-28). First
    // intake write fires scheduler.touch → first-touch catch-up of the Manual
    // backlog (that is the "at open" the operating model means).
    let handle = spawn_capture(
        sid,
        Some("/tmp"),
        &[],
        Some(root.path()),
        Some(counter.clone()),
    )
    .expect("Background open of pre-existing thread");
    let _ = wait_events(&handle, 4);
    let health_before_touch = rt
        .block_on(handle.inspect_health())
        .expect("health before first write");
    assert!(
        health_before_touch.queue.queued > 0,
        "precondition: Manual backlog still queued before first-touch write \
         (queued={})",
        health_before_touch.queue.queued
    );

    // First write → touch → catch-up.
    handle.persist(&ConversationItem::user("first-touch trigger"));
    handle.flush_blocking();
    let _ = wait_events(&handle, 5);

    // --- (3)+(4) Queue settles; first-touch absorbed backlog ---
    rt.block_on(handle.drain_settled())
        .expect("settle after first-touch");
    let health = rt
        .block_on(handle.inspect_health())
        .expect("inspect_health");
    assert_eq!(
        health.queue.queued, 0,
        "queue must settle after first-touch catch-up; queued={}",
        health.queue.queued
    );
    assert_eq!(
        health.queue.claimed, 0,
        "queue must settle after first-touch catch-up; claimed={}",
        health.queue.claimed
    );

    // --- (1) ready ≈ total when compact would trip ---
    let (ready, total) = health_ready_total(&health);
    eprintln!(
        "drain-arch: first-touch backlog_before={} → ready={ready} total={total} queue={:?}",
        health_before_touch.queue.queued, health.queue
    );
    assert!(total > 0, "expected derivations after multi-turn seed");
    assert!(
        ready * 100 >= total * 90,
        "healthy system: ready≈total at settle (ready={ready} total={total})"
    );

    // --- (2) Compact wall-time fractions of a second ---
    set_use_deterministic_inference_for_test(true);
    set_compact_mode_for_test(Some(CompactMode::Replace));
    set_compact_params_override_for_test(Some(ViewCompactParams {
        lower_bound: Some(200.0),
        percentages: Some(PartialViewProfilePercentages {
            full: Some(30.0),
            smooth: Some(25.0),
            detailed: Some(20.0),
            brief: Some(25.0),
        }),
    }));
    let t0 = Instant::now();
    let wb = rt
        .block_on(grok_lhc_host::replace_compact_for_writeback(sid))
        .expect("compact after background settle");
    let compact_wall = t0.elapsed();
    set_compact_mode_for_test(None);
    set_use_deterministic_inference_for_test(false);
    eprintln!(
        "drain-arch: compact_wall={compact_wall:?} receipt_tokens={} \
         (ref: t3code 0.4s with pre-built summaries)",
        wb.receipt_total_tokens
    );
    assert!(
        compact_wall < Duration::from_secs(2),
        "compact must be sub-second-class after background drain; took {compact_wall:?}"
    );

    handle.shutdown_blocking();
    wait_registry_gone(sid);
}

/// N3 reachability: public SDK + deterministic callbacks + tight ViewCompactParams
/// + multi-turn seed **can** emit typed bands. Contrasts with production
/// `LhcSession::compact` (`params: None`) which does not force bands on the
/// write-back fixture. If this fails, the Chunk 3 harness brief in FORK.md
/// needs revision.
#[tokio::test(flavor = "current_thread")]
async fn n3_deterministic_callbacks_with_tight_params_can_emit_typed_bands() {
    use lhc::create_deterministic_inference_callbacks;
    use lhc::init_lhc;
    use lhc::intake_stream::MessageEventInput;
    use lhc::shared_tech::derivation::{SdkConfig, SdkMode};
    use lhc::shared_tech::errors::OpResult;
    use lhc::shared_tech::view::{
        PartialViewProfilePercentages, SessionThreadViewEntry, SessionThreadViewMessage,
        ViewCompactParams,
    };
    use lhc::thread_view::CompactOpts;
    use lhc::threads::{NewThreadInput, ThreadRef};
    use serde_json::{Map, json};

    fn ev(kind: &str, payload: Map<String, serde_json::Value>, key: &str) -> MessageEventInput {
        MessageEventInput {
            event_kind: kind.into(),
            idempotency_key: Some(key.into()),
            actor: "grok".into(),
            harness: "n3-probe".into(),
            payload,
            extra: Map::new(),
        }
    }

    let root = TempDir::new().unwrap();
    let registry = root.path().join("registry.sqlite");
    let path = root.path().join("t.sqlite");
    let path_str = path.to_string_lossy().into_owned();
    let sdk = init_lhc(SdkConfig {
        inference_callbacks: Some(create_deterministic_inference_callbacks()),
        inference: None,
        mode: SdkMode::Manual,
        clock: None,
        guards: None,
        tool_result: None,
        lease: None,
        chunk_policy: None,
        view: None,
    });
    let created = sdk
        .threads
        .new_thread(NewThreadInput {
            file_path: path_str.clone(),
            title: None,
            cwd: None,
            registry_path: Some(registry.to_string_lossy().into_owned()),
        })
        .await;
    assert!(created.is_ok(), "{created:?}");

    let blob = "word ".repeat(200);
    for turn in 0..12 {
        let batch = vec![
            ev(
                "user_prompt",
                {
                    let mut m = Map::new();
                    m.insert("text".into(), json!(format!("turn {turn} {blob}")));
                    m
                },
                &format!("u{turn}"),
            ),
            ev(
                "assistant_text",
                {
                    let mut m = Map::new();
                    m.insert("text".into(), json!(format!("answer {turn} {blob}")));
                    m
                },
                &format!("a{turn}"),
            ),
            ev("turn_end", Map::new(), &format!("e{turn}")),
        ];
        let r = sdk
            .intake_stream
            .message_events(ThreadRef::file_path(&path_str), &batch)
            .await;
        assert!(r.is_ok(), "turn {turn}: {r:?}");
    }
    let _ = sdk.work.drain(ThreadRef::file_path(&path_str), None).await;

    let band_count = |v: &lhc::shared_tech::view::SessionThreadView| {
        v.entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u))
                        if u.source_messages.is_empty()
                )
            })
            .count()
    };

    // Production-shaped call: params None — must not be relied on for bands.
    let _ = sdk
        .thread_view
        .compact(
            ThreadRef::file_path(&path_str),
            CompactOpts {
                profile: None,
                params: None,
                signal: None,
            },
        )
        .await;
    let v_none = sdk
        .thread_view
        .get_session_thread_view(ThreadRef::file_path(&path_str))
        .await;
    let OpResult::Ok { value: view_none } = v_none else {
        panic!("view after params=None: {v_none:?}");
    };
    let bands_none = band_count(&view_none);

    let params = ViewCompactParams {
        lower_bound: Some(400.0),
        percentages: Some(PartialViewProfilePercentages {
            full: Some(25.0),
            smooth: Some(25.0),
            detailed: Some(25.0),
            brief: Some(25.0),
        }),
    };
    let c = sdk
        .thread_view
        .compact(
            ThreadRef::file_path(&path_str),
            CompactOpts {
                profile: None,
                params: Some(params),
                signal: None,
            },
        )
        .await;
    assert!(c.is_ok(), "tight-params compact failed: {c:?}");
    let v = sdk
        .thread_view
        .get_session_thread_view(ThreadRef::file_path(&path_str))
        .await;
    let OpResult::Ok { value: view } = v else {
        panic!("view after tight params: {v:?}");
    };
    let bands = band_count(&view);
    assert!(
        bands > 0,
        "deterministic cbs + multi-turn + tight ViewCompactParams must emit          typed bands (empty source_messages); got 0 (params=None yielded {bands_none})"
    );
}

/// M1 budget probe: production `params: None` bands only when seed tokens
/// exceed the built-in `continuation` full share (120000 × 30% = 36000).
/// Documents the size G2 must use — never shrink ViewCompactParams instead.
#[tokio::test(flavor = "current_thread")]
async fn m1_production_params_none_requires_seed_above_full_budget() {
    use lhc::create_deterministic_inference_callbacks;
    use lhc::estimate_tokens;
    use lhc::init_lhc;
    use lhc::intake_stream::MessageEventInput;
    use lhc::shared_tech::derivation::{SdkConfig, SdkMode};
    use lhc::shared_tech::errors::OpResult;
    use lhc::shared_tech::view::{SessionThreadViewEntry, SessionThreadViewMessage};
    use lhc::thread_view::CompactOpts;
    use lhc::threads::{NewThreadInput, ThreadRef};
    use serde_json::{Map, json};

    fn ev(kind: &str, payload: Map<String, serde_json::Value>, key: &str) -> MessageEventInput {
        MessageEventInput {
            event_kind: kind.into(),
            idempotency_key: Some(key.into()),
            actor: "grok".into(),
            harness: "m1-budget".into(),
            payload,
            extra: Map::new(),
        }
    }

    fn count_bands(v: &lhc::shared_tech::view::SessionThreadView) -> usize {
        v.entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u))
                        if u.source_messages.is_empty()
                )
            })
            .count()
    }

    async fn seed_and_compact(turns: usize, words: usize) -> (i64, usize /* bands */) {
        let root = TempDir::new().unwrap();
        let registry = root.path().join("registry.sqlite");
        let path = root.path().join("t.sqlite");
        let path_str = path.to_string_lossy().into_owned();
        let sdk = init_lhc(SdkConfig {
            inference_callbacks: Some(create_deterministic_inference_callbacks()),
            inference: None,
            mode: SdkMode::Manual,
            clock: None,
            guards: None,
            tool_result: None,
            lease: None,
            chunk_policy: None,
            view: None,
        });
        let created = sdk
            .threads
            .new_thread(NewThreadInput {
                file_path: path_str.clone(),
                title: None,
                cwd: None,
                registry_path: Some(registry.to_string_lossy().into_owned()),
            })
            .await;
        assert!(created.is_ok(), "{created:?}");

        let blob = "word ".repeat(words);
        let mut est = 0i64;
        for turn in 0..turns {
            let u = format!("turn {turn} {blob}");
            let a = format!("answer {turn} {blob}");
            est += estimate_tokens(&u) + estimate_tokens(&a);
            let batch = vec![
                ev(
                    "user_prompt",
                    {
                        let mut m = Map::new();
                        m.insert("text".into(), json!(u));
                        m
                    },
                    &format!("u{turn}"),
                ),
                ev(
                    "assistant_text",
                    {
                        let mut m = Map::new();
                        m.insert("text".into(), json!(a));
                        m
                    },
                    &format!("a{turn}"),
                ),
                ev("turn_end", Map::new(), &format!("e{turn}")),
            ];
            let r = sdk
                .intake_stream
                .message_events(ThreadRef::file_path(&path_str), &batch)
                .await;
            assert!(r.is_ok(), "turn {turn}: {r:?}");
        }
        let _ = sdk.work.drain(ThreadRef::file_path(&path_str), None).await;
        let c = sdk
            .thread_view
            .compact(
                ThreadRef::file_path(&path_str),
                CompactOpts {
                    profile: None,
                    params: None,
                    signal: None,
                },
            )
            .await;
        assert!(c.is_ok(), "params=None compact failed: {c:?}");
        let v = sdk
            .thread_view
            .get_session_thread_view(ThreadRef::file_path(&path_str))
            .await;
        let OpResult::Ok { value: view } = v else {
            panic!("view: {v:?}");
        };
        (est, count_bands(&view))
    }

    const FULL_BUDGET: f64 = 120_000.0 * 30.0 / 100.0; // continuation
    let (small_tok, small_bands) = seed_and_compact(12, 200).await;
    eprintln!("M1 small seed: tokens≈{small_tok} bands={small_bands} full_budget={FULL_BUDGET}");
    assert_eq!(
        small_bands, 0,
        "12×200-word seed must stay under production full budget (got bands)"
    );
    assert!((small_tok as f64) < FULL_BUDGET);

    // Prefer fewer, larger turns so real-inference G2 has fewer PromptSmoothing
    // round-trips while still clearing the production full budget.
    let (big_tok, big_bands) = seed_and_compact(6, 5000).await;
    eprintln!(
        "M1 large seed (6×5000): tokens≈{big_tok} bands={big_bands} full_budget={FULL_BUDGET}"
    );
    assert!(
        (big_tok as f64) > FULL_BUDGET,
        "6×5000-word seed must exceed full_budget={FULL_BUDGET} (got {big_tok})"
    );
    assert!(
        big_bands > 0,
        "M1: production params:None must emit typed bands once seed exceeds \
         continuation full_budget={FULL_BUDGET}; got 0 (tokens≈{big_tok}). \
         If this fails, G2 cannot certify under real budgets without shrinking params."
    );
}

/// Banded LHC-ahead crash window (adapter-reachable).
///
/// Uses deterministic callbacks + multi-turn seed + tight `ViewCompactParams`
/// to emit typed bands, then applies write-back via `replace_history` while
/// native is still the old body (LHC ahead / native behind). Retry must not
/// double-record band summary text.
///
/// Would fail if the second `replace_history` minted a second summary key.
#[test]
fn writeback_crash_between_lhc_compact_and_native_replace_is_transient() {
    use lhc::create_deterministic_inference_callbacks;
    use lhc::init_lhc;
    use lhc::intake_stream::MessageEventInput;
    use lhc::shared_tech::derivation::{SdkConfig, SdkMode};
    use lhc::shared_tech::errors::OpResult;
    use lhc::shared_tech::view::{
        PartialViewProfilePercentages, SessionThreadViewEntry, SessionThreadViewMessage,
        ViewCompactParams,
    };
    use lhc::thread_view::CompactOpts;
    use lhc::threads::{NewThreadInput, ThreadRef};
    use serde_json::{Map, json};

    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-wb-lhc-ahead";
    std::fs::create_dir_all(root.path().join("threads")).unwrap();

    let registry = root.path().join("registry.sqlite");
    let path = thread_file_path(root.path(), sid);
    let path_str = path.to_string_lossy().into_owned();
    let registry_str = registry.to_string_lossy().into_owned();

    let band_texts = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            fn ev(
                kind: &str,
                payload: Map<String, serde_json::Value>,
                key: &str,
            ) -> MessageEventInput {
                MessageEventInput {
                    event_kind: kind.into(),
                    idempotency_key: Some(key.into()),
                    actor: "grok".into(),
                    harness: "n3-banded".into(),
                    payload,
                    extra: Map::new(),
                }
            }

            let sdk = init_lhc(SdkConfig {
                inference_callbacks: Some(create_deterministic_inference_callbacks()),
                inference: None,
                mode: SdkMode::Manual,
                clock: None,
                guards: None,
                tool_result: None,
                lease: None,
                chunk_policy: None,
                view: None,
            });
            assert!(
                sdk.threads
                    .new_thread(NewThreadInput {
                        file_path: path_str.clone(),
                        title: None,
                        cwd: None,
                        registry_path: Some(registry_str.clone()),
                    })
                    .await
                    .is_ok()
            );
            let blob = "word ".repeat(200);
            for turn in 0..12 {
                let batch = vec![
                    ev(
                        "user_prompt",
                        {
                            let mut m = Map::new();
                            m.insert("text".into(), json!(format!("turn {turn} {blob}")));
                            m
                        },
                        &format!("u{turn}"),
                    ),
                    ev(
                        "assistant_text",
                        {
                            let mut m = Map::new();
                            m.insert("text".into(), json!(format!("answer {turn} {blob}")));
                            m
                        },
                        &format!("a{turn}"),
                    ),
                    ev("turn_end", Map::new(), &format!("e{turn}")),
                ];
                assert!(
                    sdk.intake_stream
                        .message_events(ThreadRef::file_path(&path_str), &batch)
                        .await
                        .is_ok()
                );
            }
            let _ = sdk.work.drain(ThreadRef::file_path(&path_str), None).await;
            let params = ViewCompactParams {
                lower_bound: Some(400.0),
                percentages: Some(PartialViewProfilePercentages {
                    full: Some(25.0),
                    smooth: Some(25.0),
                    detailed: Some(25.0),
                    brief: Some(25.0),
                }),
            };
            let compact = sdk
                .thread_view
                .compact(
                    ThreadRef::file_path(&path_str),
                    CompactOpts {
                        profile: None,
                        params: Some(params),
                        signal: None,
                    },
                )
                .await;
            assert!(compact.is_ok(), "banded compact failed: {compact:?}");
            let view_pre = sdk
                .thread_view
                .get_session_thread_view(ThreadRef::file_path(&path_str))
                .await;
            let OpResult::Ok { value: banded_view } = view_pre else {
                panic!("session view after banded compact: {view_pre:?}");
            };
            let texts: Vec<String> = banded_view
                .entries
                .iter()
                .filter_map(|e| match e {
                    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u))
                        if u.source_messages.is_empty() =>
                    {
                        Some(u.content.clone())
                    }
                    _ => None,
                })
                .collect();
            assert!(
                !texts.is_empty(),
                "typed bands required for LHC-ahead window; got 0 empty-source Users"
            );
            // Drain/close before capture opens the same sqlite files.
            sdk.drain_settled(ThreadRef::file_path(&path_str)).await;
            texts
        })
    };

    // Native still pre-compact (replace never ran).
    let mut old_native = vec![ConversationItem::system("sys")];
    for i in 0..3 {
        let mut u = ConversationItem::user(format!("turn {i} stale-native"));
        u.set_prompt_index(i);
        old_native.push(u);
        old_native.push(ConversationItem::assistant(format!("a{i}")));
    }
    let handle = spawn_capture(sid, Some("/tmp"), &old_native, Some(root.path()), None)
        .expect("spawn on pre-banded thread");
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(100));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (rt_view, rt_kinds) = rt
        .block_on(handle.get_classify_context())
        .expect("classify context through capture");
    let body = build_writeback_conversation(&old_native, &rt_view, &rt_kinds)
        .expect("write-back from banded view");
    let meta_with_band = body
        .iter()
        .filter(|i| {
            matches!(i, ConversationItem::User(u) if u.synthetic_reason.is_some())
                && band_texts.iter().any(|b| i.text_content().contains(b))
        })
        .count();
    assert!(
        meta_with_band >= 1,
        "write-back body must include band user_meta from empty-source entries"
    );

    handle.replace_history(&body);
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let once = handle.list_events_blocking().unwrap();
    let once_keys = keys(&once);
    let summary_hits = |ev: &[EventRecord]| {
        ev.iter()
            .filter(|e| {
                e.text_payload()
                    .is_some_and(|p| band_texts.iter().any(|b| p.text.contains(b)))
            })
            .count()
    };
    let hits1 = summary_hits(&once);
    assert!(
        hits1 >= 1,
        "first write-back after LHC-ahead compact must record band summary text"
    );

    handle.replace_history(&body);
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let again = handle.list_events_blocking().unwrap();
    assert_eq!(
        summary_hits(&again),
        hits1,
        "retry after LHC-ahead window must not double-record band summaries"
    );
    assert_eq!(keys(&again), once_keys);
    handle.shutdown_blocking();
}

/// Write-back through the native compaction replace path decreases
/// `get_estimated_total_tokens()` (the accounting self-correct claim).
#[tokio::test(flavor = "multi_thread")]
async fn writeback_replace_decreases_estimated_total_tokens() {
    let (native, body) = writeback_fixture();
    // Inflate the native body so the compacted write-back is unambiguously smaller.
    let mut fat = native;
    fat.push(ConversationItem::user("x".repeat(40_000)));
    fat.push(ConversationItem::assistant("y".repeat(40_000)));
    let before_est = estimate_conversation_tokens(&fat);
    let after_est = estimate_conversation_tokens(&body);
    assert!(
        after_est < before_est,
        "fixture invalid: write-back ({after_est}) not smaller than native ({before_est})"
    );

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let token = CancellationToken::new();
    let config = SamplingConfig {
        base_url: "https://api.example.com".into(),
        model: "test-model".into(),
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        api_backend: Default::default(),
        extra_headers: Default::default(),
        query_params: Default::default(),
        env_http_headers: Default::default(),
        context_window: std::num::NonZeroU64::new(128_000).unwrap(),
        reasoning_effort: None,
        stream_tool_calls: None,
    };
    let handle = ChatStateActor::spawn(fat, config, Box::new(NullChatPersistence), event_tx, token);
    let _ = handle.get_conversation().await;
    let tokens_before = handle.get_estimated_total_tokens().await;
    handle.replace_conversation_for_compaction(body);
    let tokens_after = handle.get_estimated_total_tokens().await;
    assert!(
        tokens_after < tokens_before,
        "get_estimated_total_tokens must decrease after write-back replace \
         (before={tokens_before}, after={tokens_after})"
    );
}

// ── Chunk 2: inference / serving / compact / watchdog ─────────────────

/// Sampler passed to spawn_capture is registered under the session id.
#[tokio::test(flavor = "multi_thread")]
async fn chunk2_mock_sampler_registered_at_spawn() {
    use grok_lhc_host::LhcInferenceSampler;
    let sid = "cert-chunk2-mock-sampler";
    let root = TempDir::new().unwrap();
    let mock: Arc<dyn LhcInferenceSampler> = Arc::new(MockLhcInferenceSampler::new());
    let handle = spawn_capture(
        sid,
        Some("/tmp"),
        &[],
        Some(root.path()),
        Some(Arc::clone(&mock)),
    )
    .unwrap();
    assert!(
        inference_sampler_registered(sid),
        "spawn_capture must register the sampler"
    );
    let sample = mock
        .sample(
            LhcInferenceRequest::SmoothPrompt {
                text: "ping".into(),
                max_output_tokens: 64,
            },
            CancellationToken::new(),
        )
        .await
        .expect("mock sample");
    assert!(sample.text.contains("smooth_prompt"));
    assert_eq!(sample.model, "mock-lhc-inference");
    assert!(!sample.request_messages.is_empty());
    let handle2 = handle.clone();
    tokio::task::spawn_blocking(move || {
        handle2.flush_blocking();
        handle2.shutdown_blocking();
    })
    .await
    .unwrap();
    wait_registry_gone(sid);
    assert!(!inference_sampler_registered(sid));
}

/// Counting sampler exercises every op + target token forwarding via callbacks.
#[tokio::test(flavor = "multi_thread")]
async fn chunk2_inference_all_ops_via_counting_double() {
    use grok_lhc_host::{
        LhcInferenceOp, inference_callbacks_for_session, register_inference_sampler,
        unregister_inference_sampler,
    };
    use lhc::shared_tech::{
        CompressDetailedTurnInput, SmoothPromptInput, SummarizeChunkBriefInput,
        SummarizeToolResultInput,
    };
    let sid = "cert-chunk2-count-ops";
    let counter = Arc::new(CountingLhcInferenceSampler::new());
    register_inference_sampler(sid, counter.clone());
    let cbs = inference_callbacks_for_session(sid);
    let _ = (cbs.smooth_prompt)(SmoothPromptInput { text: "a".into() }).await;
    let _ = (cbs.summarize_tool_result)(SummarizeToolResultInput {
        tool_name: "bash".into(),
        content: "out".into(),
        outcome: None,
        target_tokens: Some(256),
        operation_class: None,
        response_shape: None,
        prompt_mode: None,
        facts: None,
    })
    .await;
    let _ = (cbs.compress_detailed_turn)(CompressDetailedTurnInput {
        dialogue_text: "d".into(),
        input_tokens: 10,
        target_min_tokens: 1,
        target_aim_tokens: 2,
        target_max_tokens: 128,
    })
    .await;
    let _ = (cbs.summarize_chunk_brief)(SummarizeChunkBriefInput {
        text: "b".into(),
        input_tokens: 10,
        target_min_tokens: 1,
        target_aim_tokens: 2,
        target_max_tokens: 64,
    })
    .await;
    let recorded = counter.call_ops();
    assert_eq!(
        recorded,
        vec![
            LhcInferenceOp::SmoothPrompt,
            LhcInferenceOp::SummarizeToolResult,
            LhcInferenceOp::CompressDetailedTurn,
            LhcInferenceOp::SummarizeChunkBrief,
        ]
    );
    let tokens: Vec<u32> = counter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, t)| *t)
        .collect();
    assert_eq!(tokens[1], 256);
    assert_eq!(tokens[2], 128);
    assert_eq!(tokens[3], 64);
    unregister_inference_sampler(sid);
}

/// Serving fails open when capture is inactive (no env re-read required).
#[tokio::test(flavor = "current_thread")]
async fn chunk2_serve_fail_open_when_inactive() {
    let native = vec![ConversationItem::user("keep-me")];
    let decision = serve_request_context("no-such-session", &native, None).await;
    let (items, substituted) = apply_serve_decision(native, decision);
    assert!(!substituted);
    assert_eq!(items.len(), 1);
}

/// Live capture → get_llm_request_context → substitute preserves system prefix.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn chunk2_serve_substitutes_from_live_context() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-chunk2-serve";
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.persist(&ConversationItem::user("hello-from-host"));
    handle.persist(&ConversationItem::assistant("hi-back"));
    let handle2 = handle.clone();
    tokio::task::spawn_blocking(move || {
        handle2.flush_blocking();
        wait_events(&handle2, 2);
    })
    .await
    .unwrap();

    let native = vec![
        ConversationItem::system("host-system"),
        ConversationItem::user("stale-native"),
    ];
    let decision = serve_request_context(sid, &native, None).await;
    let (items, substituted) = apply_serve_decision(native, decision);
    assert!(
        substituted,
        "expected Substitute after live capture; got Native"
    );
    assert!(matches!(&items[0], ConversationItem::System(_)));
    assert!(!body_has_tool_cycle(&items));

    tokio::task::spawn_blocking(move || handle.shutdown_blocking())
        .await
        .unwrap();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Serving times out to native when the capture worker is blocked.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn chunk2_serve_timeout_falls_open_on_blocked_worker() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-chunk2-serve-timeout";
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let handle_block = handle.clone();
    tokio::task::spawn_blocking(move || {
        let entered = handle_block.block_worker(release_rx);
        let _ = entered.blocking_recv();
    })
    .await
    .unwrap();
    let native = vec![ConversationItem::user("blocked")];
    let started = std::time::Instant::now();
    let decision = serve_request_context(sid, &native, None).await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "serve must not hang indefinitely"
    );
    assert!(
        matches!(
            decision,
            ServeDecision::Native {
                reason: "get_classify_context_timeout"
            }
        ),
        "expected timeout native, got {decision:?}"
    );
    let _ = release_tx.send(());
    tokio::task::spawn_blocking(move || handle.shutdown_blocking())
        .await
        .unwrap();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Compact modes are mutually exclusive writers (env + test override).
#[test]
fn chunk2_compact_modes_mutually_exclusive() {
    let _g = env_lock();
    set_compact_mode_for_test(Some(CompactMode::Shadow));
    assert!(resolve_compact_mode().native_writes());
    assert!(!resolve_compact_mode().lhc_writes());
    set_compact_mode_for_test(Some(CompactMode::Replace));
    assert!(!resolve_compact_mode().native_writes());
    assert!(resolve_compact_mode().lhc_writes());
    set_compact_mode_for_test(Some(CompactMode::Off));
    assert!(resolve_compact_mode().native_writes());
    assert!(!resolve_compact_mode().lhc_writes());
    set_compact_mode_for_test(None);
}

/// Bridge state machine: exactly one LHC attempt per event shape.
#[test]
fn chunk2_compact_bridge_one_attempt_per_event() {
    // success
    let mut ok = CompactEventBridge::new(CompactMode::Replace);
    assert!(ok.should_attempt_replace());
    ok.record_replace_result(true);
    assert!(!ok.should_attempt_replace());
    assert_eq!(ok.lhc_attempts(), 1);
    assert!(ok.lhc_wrote());

    // first-fails → sticky fail-open (native); no second attempt
    let mut fo = CompactEventBridge::new(CompactMode::Replace);
    fo.record_replace_result(false);
    assert!(fo.fail_open());
    assert!(!fo.should_attempt_replace());
    assert_eq!(fo.lhc_attempts(), 1);
    assert!(matches!(
        fo.choke_action(),
        grok_lhc_host::CompactChokeAction::RunNative
    ));

    // both-fail shape is the same sticky machine (caller must not re-enter)
    let mut both = CompactEventBridge::new(CompactMode::Replace);
    both.record_replace_result(false);
    assert!(!both.should_attempt_replace());
}

/// Shadow preview is counted once when invoked.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn chunk2_shadow_preview_is_counted() {
    let _g = env_lock();
    reset_compact_call_counters();
    set_compact_mode_for_test(Some(CompactMode::Shadow));
    let root = TempDir::new().unwrap();
    let sid = "cert-chunk2-shadow";
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    // Give open time.
    thread::sleep(Duration::from_millis(100));
    shadow_preview_compact(sid).await;
    assert_eq!(preview_call_count(), 1);
    assert_eq!(replace_call_count(), 0);
    set_compact_mode_for_test(None);
    tokio::task::spawn_blocking(move || handle.shutdown_blocking())
        .await
        .unwrap();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Mid-tool-cycle native body is replaced wholesale (never hybrid).
#[test]
fn chunk2_mid_tool_cycle_all_lhc_or_native() {
    use xai_grok_sampling_types::ToolCall;
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }]),
    ];
    assert!(body_has_tool_cycle(&native[1..]));
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![view_user("run", "u")],
    };
    match decide_substitution(
        &native,
        &view,
        &SourceKindIndex::assume_sourced_users_are_prompts(&view),
        None,
    ) {
        ServeDecision::Substitute { items } => {
            assert!(!body_has_tool_cycle(&items));
            assert!(matches!(&items[0], ConversationItem::System(_)));
        }
        ServeDecision::Native { reason } => panic!("expected all-LHC substitute, got {reason}"),
    }
}

/// Out-of-thread watchdog: suspect body runs on a worker thread; controller
/// uses wall-clock recv_timeout. Limitation #2 remains documented as open in
/// FORK.md until CI proves hang detection.
#[test]
fn chunk2_async_guard_out_of_thread_watchdog() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-chunk2-watchdog";
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let root_path = root.path().to_path_buf();
    thread::spawn(move || {
        with_lhc_env(&root_path, || {
            let tee = tee_chat_persistence(sid, "/tmp", &[], Box::new(NullChatPersistence), None);
            for _ in 0..100 {
                if capture_active(sid) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(capture_active(sid));
            drop(tee);
            wait_registry_gone(sid);
        });
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("out-of-thread watchdog: tee path hung (>10s)");
}

// ── Chunk 2 G1: hook-4 equivalence instrumentation ───────────────────

/// Text-only window: both divergence counters stay silent when native == served
/// (substituted). Compared-turn counter advances.
#[test]
fn equiv_text_only_window_both_silent() {
    let _g = env_lock();
    reset_equivalence_counters();
    let body = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("hi"),
        ConversationItem::assistant("hello"),
    ];
    let report = compare_serve_equivalence(&body, &body);
    assert!(!report.structural_divergence);
    assert!(!report.informational_divergence);
    let obs = observe_serve_equivalence("equiv-text", Some(0), false, true, &body, &body);
    assert!(obs.compared);
    assert!(!obs.fallback);
    assert_eq!(structural_hit_count(), 0);
    assert_eq!(informational_hit_count(), 0);
    assert_eq!(serve_compared_turns(), 1);
    assert_eq!(serve_fallback_turns(), 0);
    let snap = equivalence_snapshot();
    assert_eq!(snap.turns_served_and_compared, 1);
    assert_eq!(snap.turns_fallen_back, 0);
    assert_eq!(snap.structural_divergences, 0);
    assert_eq!(snap.informational_divergences, 0);
}

/// Tool-using window: structural fires; informational silent when projection matches.
#[test]
fn equiv_tool_window_structural_only() {
    let _g = env_lock();
    reset_equivalence_counters();
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run"),
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: "calling".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
        ConversationItem::tool_result("c1", "file_a"),
    ];
    // Served side already in LHC text shape (what faithful serving produces).
    let served = project_conversation_canonical(&native)
        .into_iter()
        .map(|p| match p.role {
            "system" => ConversationItem::system(p.text),
            "user" => ConversationItem::user(p.text),
            "assistant" => ConversationItem::assistant(p.text),
            other => panic!("unexpected role {other}"),
        })
        .collect::<Vec<_>>();
    let report = compare_serve_equivalence(&native, &served);
    assert!(
        report.structural_divergence,
        "tool window must be structurally divergent"
    );
    assert!(
        !report.informational_divergence,
        "faithful tool projection must be informationally equal — got diff at {:?}",
        report.first_info_diff_index
    );
    let obs = observe_serve_equivalence("equiv-tools", Some(1), false, true, &native, &served);
    assert!(obs.compared);
    assert_eq!(structural_hit_count(), 1);
    assert_eq!(informational_hit_count(), 0);
    assert_eq!(serve_compared_turns(), 1);
}

/// S3 — native provider-raw vs **real** `decide_substitution` translator path.
/// Cosmetic pretty-vs-compact ⇒ both channels silent.
#[test]
fn equiv_tool_arg_cosmetic_formatting_silent_different_paths() {
    let _g = env_lock();
    reset_equivalence_counters();
    const PRETTY: &str = "{\n  \"cmd\": \"ls\",\n  \"timeout_ms\": 5000\n}";
    const COMPACT: &str = r#"{"cmd":"ls","timeout_ms":5000}"#;
    assert_ne!(PRETTY, COMPACT);
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run"),
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: PRETTY.into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            view_user("run", "u"),
            view_assistant_tool("bash", COMPACT, "a"),
        ],
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&view);
    let served = match decide_substitution(&native, &view, &kinds, None) {
        ServeDecision::Substitute { items } => items,
        ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
    };
    match served
        .iter()
        .find(|i| matches!(i, ConversationItem::Assistant(_)))
    {
        Some(ConversationItem::Assistant(a)) => {
            assert!(
                !a.tool_calls.is_empty(),
                "translator must conserve tool_calls"
            );
            assert_eq!(a.tool_calls[0].arguments.as_ref(), COMPACT);
        }
        other => panic!("expected translated Assistant with tools, got {other:?}"),
    }
    let report = compare_serve_equivalence(&native, &served);
    assert!(!report.structural_divergence);
    assert!(
        !report.informational_divergence,
        "cosmetic JSON via translator must be silent — diff at {:?}",
        report.first_info_diff_index
    );
    let obs = observe_serve_equivalence("equiv-tool-fmt", Some(4), true, true, &native, &served);
    assert!(obs.compared);
    assert_eq!(informational_hit_count(), 0);
}

/// S3 — real argument change: native vs translator-built substitute.
#[test]
fn equiv_tool_arg_real_change_informational_different_paths() {
    let _g = env_lock();
    reset_equivalence_counters();
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run"),
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"cmd":"ls"}"#.into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            view_user("run", "u"),
            view_assistant_tool("bash", r#"{"cmd":"pwd"}"#, "a"),
        ],
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&view);
    let served = match decide_substitution(&native, &view, &kinds, None) {
        ServeDecision::Substitute { items } => items,
        ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
    };
    let report = compare_serve_equivalence(&native, &served);
    assert!(report.informational_divergence);
    assert!(report.structural_divergence);
    let obs = observe_serve_equivalence("equiv-tool-arg", Some(5), true, true, &native, &served);
    assert!(obs.compared);
    assert_eq!(informational_hit_count(), 1);
}

/// S3 — swapped tool name through translator registers structurally.
#[test]
fn equiv_swapped_tool_call_registers_structurally() {
    let _g = env_lock();
    reset_equivalence_counters();
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run"),
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"cmd":"ls"}"#.into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            view_user("run", "u"),
            view_assistant_tool("python", r#"{"cmd":"ls"}"#, "a"),
        ],
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&view);
    let served = match decide_substitution(&native, &view, &kinds, None) {
        ServeDecision::Substitute { items } => items,
        ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
    };
    let report = compare_serve_equivalence(&native, &served);
    assert!(
        report.structural_divergence,
        "swapped tool name via translator must register structurally"
    );
    let obs = observe_serve_equivalence("equiv-tool-swap", Some(6), true, true, &native, &served);
    assert!(obs.compared);
    assert_eq!(structural_hit_count(), 1);
}

/// S2 — object key reorder silent (instrument sorted-key canonicalize).
#[test]
fn equiv_tool_arg_object_key_reorder_silent() {
    let _g = env_lock();
    reset_equivalence_counters();
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run"),
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"a":1,"b":2}"#.into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            view_user("run", "u"),
            view_assistant_tool("bash", r#"{"b":2,"a":1}"#, "a"),
        ],
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&view);
    let served = match decide_substitution(&native, &view, &kinds, None) {
        ServeDecision::Substitute { items } => items,
        ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
    };
    let report = compare_serve_equivalence(&native, &served);
    assert!(!report.structural_divergence);
    assert!(!report.informational_divergence);
}

/// S2 — array element reorder divergent.
#[test]
fn equiv_tool_arg_array_reorder_divergent() {
    let _g = env_lock();
    reset_equivalence_counters();
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run"),
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"xs":[1,2]}"#.into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            view_user("run", "u"),
            view_assistant_tool("bash", r#"{"xs":[2,1]}"#, "a"),
        ],
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&view);
    let served = match decide_substitution(&native, &view, &kinds, None) {
        ServeDecision::Substitute { items } => items,
        ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
    };
    let report = compare_serve_equivalence(&native, &served);
    assert!(report.informational_divergence);
    assert!(report.structural_divergence);
}

/// Informational counter fires on a real content mismatch (text-only window).
#[test]
fn equiv_informational_fires_on_content_mismatch() {
    let _g = env_lock();
    reset_equivalence_counters();
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("alpha"),
    ];
    let served = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("beta"),
    ];
    let report = compare_serve_equivalence(&native, &served);
    assert!(report.structural_divergence);
    assert!(report.informational_divergence);
    let obs = observe_serve_equivalence("equiv-mismatch", Some(2), true, true, &native, &served);
    assert!(obs.compared);
    assert_eq!(informational_hit_count(), 1);
    assert_eq!(serve_compared_turns(), 1);
}

/// L4 — post-write-back native (collapsed bands) vs serving (N band items):
/// structural may fire; informational must stay silent when band text matches.
#[test]
fn equiv_post_writeback_band_collapse_informational_silent() {
    let _g = env_lock();
    reset_equivalence_counters();
    let (native_pre, writeback) = writeback_fixture();
    let view = realistic_post_compact_view();
    let kinds = realistic_kinds(&view);
    let served = match decide_substitution(&native_pre, &view, &kinds, None) {
        ServeDecision::Substitute { items } => items,
        ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
    };
    let report = compare_serve_equivalence(&writeback, &served);
    assert!(
        report.structural_divergence,
        "collapsed vs N-band representation must remain structural"
    );
    assert!(
        !report.informational_divergence,
        "band collapse must not poison informational evidence — diff at {:?}",
        report.first_info_diff_index
    );
    let obs = observe_serve_equivalence("equiv-wb-bands", Some(3), true, true, &writeback, &served);
    assert!(obs.compared);
    assert_eq!(informational_hit_count(), 0);
    assert_eq!(serve_compared_turns(), 1);
}

/// Q3 — missing band vs write-back must fire informational (not just structural).
/// Would fail if band collapse projected a constant token.
#[test]
fn equiv_band_collapse_missing_band_is_informational() {
    let writeback = vec![
        ConversationItem::system("sys"),
        ConversationItem::user_meta(
            "[context · brief]\nA\n\n[context · detailed]\nB\n\n[context · smooth]\nC",
        ),
        ConversationItem::user("live"),
    ];
    let served = vec![
        ConversationItem::system("sys"),
        ConversationItem::user_meta("[context · brief]\nA"),
        ConversationItem::user_meta("[context · smooth]\nC"),
        ConversationItem::user("live"),
    ];
    let report = compare_serve_equivalence(&writeback, &served);
    assert!(
        report.informational_divergence,
        "missing band must register informational divergence"
    );
}

/// Q3 — reordered bands must fire informational divergence.
#[test]
fn equiv_band_collapse_reordered_bands_is_informational() {
    let writeback = vec![
        ConversationItem::system("sys"),
        ConversationItem::user_meta(
            "[context · brief]\nA\n\n[context · detailed]\nB\n\n[context · smooth]\nC",
        ),
        ConversationItem::user("live"),
    ];
    let served = vec![
        ConversationItem::system("sys"),
        ConversationItem::user_meta("[context · detailed]\nB"),
        ConversationItem::user_meta("[context · brief]\nA"),
        ConversationItem::user_meta("[context · smooth]\nC"),
        ConversationItem::user("live"),
    ];
    let report = compare_serve_equivalence(&writeback, &served);
    assert!(
        report.informational_divergence,
        "reordered bands must register informational divergence"
    );
}

/// K1 — fail-open must not count as compared / zero-divergence evidence.
#[test]
fn equiv_fail_open_turn_not_counted_as_compared() {
    let _g = env_lock();
    reset_equivalence_counters();
    let body = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("keep-native"),
    ];
    // Fail-open: served == native, but substituted=false.
    let obs = observe_serve_equivalence("equiv-fallback", Some(0), false, false, &body, &body);
    assert!(!obs.compared, "fail-open must not be a compared turn");
    assert!(obs.fallback);
    assert!(!obs.structural_divergence);
    assert!(!obs.informational_divergence);
    assert_eq!(
        structural_hit_count(),
        0,
        "fail-open must not increment structural"
    );
    assert_eq!(
        informational_hit_count(),
        0,
        "fail-open must not increment informational"
    );
    assert_eq!(
        serve_compared_turns(),
        0,
        "fail-open must not be recorded as compared"
    );
    assert_eq!(serve_fallback_turns(), 1);
    let snap = equivalence_snapshot();
    assert_eq!(snap.turns_served_and_compared, 0);
    assert_eq!(snap.turns_fallen_back, 1);
    assert_eq!(snap.structural_divergences, 0);
    assert_eq!(snap.informational_divergences, 0);
}

// ── Chunk 3A: config / status / health / mid-session opt-in ─────────────

#[test]
fn chunk3a_config_env_wins_over_file_enabled() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    unsafe { std::env::set_var("GROK_LHC", "0") };
    let r = resolve_lhc_config(&LhcFileConfig {
        enabled: Some(true),
        ..Default::default()
    });
    assert!(!r.enabled.value, "env 0 must beat config enabled=true");
    assert_eq!(r.enabled.source, grok_lhc_host::ConfigSource::Env);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
}

#[test]
fn chunk3a_config_file_enables_when_env_unset() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    unsafe { std::env::remove_var("GROK_LHC") };
    let r = resolve_lhc_config(&LhcFileConfig {
        enabled: Some(true),
        ..Default::default()
    });
    assert!(r.enabled.value);
    assert_eq!(r.enabled.source, grok_lhc_host::ConfigSource::ConfigFile);
    // apply must set env so is_enabled() sees it
    apply_resolved_config(&r);
    assert!(is_enabled());
    unsafe { std::env::remove_var("GROK_LHC") };
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
}

#[test]
fn chunk3a_status_off_reports_native_engine() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    unsafe { std::env::remove_var("GROK_LHC") };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let r = rt.block_on(status_report("chunk3a-status-off"));
    assert!(!r.enabled);
    assert_eq!(r.context_engine, ContextEngine::Native);
    let text = format_status_report(&r);
    assert!(text.contains("Active context engine:** native"));
    assert!(text.contains("off"));
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
}

#[test]
fn chunk3a_status_on_healthy_store() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "chunk3a-status-on";
    clear_last_serve_outcome(sid);
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("hi"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let _ = wait_events(&handle, 1);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let r = rt.block_on(status_report(sid));
    assert!(r.enabled);
    assert!(r.capture_active);
    // Capture alone must not claim LHC is the active engine.
    assert_eq!(r.context_engine, ContextEngine::NoServeTurnYet);
    assert!(r.event_count.unwrap_or(0) >= 1);
    assert!(r.health.worker_alive);
    let h = rt.block_on(health_check(sid));
    assert!(h.worker_alive);
    assert!(h.storage_reachable);
    // After a real serve consult, engine follows the decision.
    let decision = rt.block_on(serve_request_context(sid, &native, None));
    let r2 = rt.block_on(status_report(sid));
    match decision {
        ServeDecision::Substitute { .. } => {
            assert_eq!(r2.context_engine, ContextEngine::Lhc);
        }
        ServeDecision::Native { reason } => {
            assert_eq!(r2.context_engine, ContextEngine::Native);
            assert_eq!(r2.last_serve_reason, Some(reason));
        }
    }
    shutdown_session(sid);
    wait_registry_gone(sid);
    clear_last_serve_outcome(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

#[test]
fn chunk3a_health_broken_missing_root() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let missing = std::env::temp_dir().join(format!("lhc-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", &missing);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(health_check("chunk3a-broken"));
    assert!(!h.worker_alive);
    assert!(
        !h.storage_reachable,
        "missing root must not report storage_reachable"
    );
    assert!(!h.ok, "broken store must not be healthy");
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

#[test]
fn chunk3a_mid_session_disable_and_reenable() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "chunk3a-mid-toggle";
    clear_last_serve_outcome(sid);
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("one"),
    ];
    let h1 = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let n1 = wait_events(&h1, 1).len();
    assert!(capture_active(sid));
    shutdown_session(sid);
    wait_registry_gone(sid);
    assert!(!capture_active(sid));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let r_off = rt.block_on(status_report(sid));
    assert_eq!(r_off.context_engine, ContextEngine::Native);
    let native2 = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("one"),
        ConversationItem::assistant("ack"),
        ConversationItem::user("two"),
    ];
    let h2 = spawn_capture(sid, Some("/tmp"), &native2, Some(root.path()), None).unwrap();
    let n2 = wait_events(&h2, n1).len();
    assert!(n2 >= n1, "re-enable must keep prior events (dedup)");
    assert!(capture_active(sid));
    let plan = rt.block_on(plan_repair(sid));
    assert!(!plan.actions.is_empty());
    let _ = rt.block_on(execute_repair(sid, "noop"));
    // Confirm without a fresh plan must refuse.
    let unbound = rt.block_on(execute_repair(sid, "noop"));
    assert!(
        unbound.is_err(),
        "second confirm without re-display must fail"
    );
    shutdown_session(sid);
    wait_registry_gone(sid);
    clear_last_serve_outcome(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Y1 Probe A — product path: tee installed while LHC-off, then `/lhc on`
/// (`spawn_capture`) must make subsequent persists reach the event log.
#[test]
fn chunk3a_tee_mid_session_on_from_spawned_off() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::remove_var("GROK_LHC");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "chunk3a-probe-a";
    let (mock, _rx) = xai_chat_state::MockChatPersistence::new();
    // Spawn-off: resolving tee installed, no worker.
    let mut tee = tee_chat_persistence(sid, "/tmp", &[], Box::new(mock), None);
    assert!(!capture_active(sid));
    tee.persist_message(&ConversationItem::user("before-on"));
    tee.flush();
    // Mid-session enable (product `/lhc on` path).
    unsafe { std::env::set_var("GROK_LHC", "1") };
    let handle = spawn_capture(
        sid,
        Some("/tmp"),
        &[ConversationItem::user("bootstrap")],
        Some(root.path()),
        None,
    )
    .unwrap();
    let n_boot = wait_events(&handle, 1).len();
    tee.persist_message(&ConversationItem::user("after-on-1"));
    tee.persist_message(&ConversationItem::user("after-on-2"));
    tee.flush();
    let n_after = wait_events(&handle, n_boot + 2).len();
    assert!(
        n_after >= n_boot + 2,
        "Probe A: persists after /lhc on must grow the log (boot={n_boot} after={n_after})"
    );
    drop(tee);
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Y1 Probe B — product path: tee survives `/lhc off` + `/lhc on` and does
/// not latch permanently stopped; new turns keep appending.
#[test]
fn chunk3a_tee_off_then_on_keeps_capturing() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "chunk3a-probe-b";
    assert!(
        is_enabled(),
        "Probe B requires GROK_LHC enabled at tee install"
    );
    let (mock, _rx) = xai_chat_state::MockChatPersistence::new();
    let mut tee = tee_chat_persistence(
        sid,
        "/tmp",
        &[ConversationItem::user("boot")],
        Box::new(mock),
        None,
    );
    let h1 = lookup_session(sid).unwrap_or_else(|| {
        panic!(
            "worker at spawn-on (enabled={} root={:?})",
            is_enabled(),
            root.path()
        )
    });
    let n1 = wait_events(&h1, 1).len();
    tee.persist_message(&ConversationItem::user("while-on"));
    tee.flush();
    let n2 = wait_events(&h1, n1 + 1).len();
    assert!(n2 > n1);
    // `/lhc off` — do not drop the tee (product keeps ChatStateActor persistence).
    shutdown_session(sid);
    wait_registry_gone(sid);
    assert!(!capture_active(sid));
    // `/lhc on` — same tee object must re-resolve the new handle.
    let h2 = spawn_capture(
        sid,
        Some("/tmp"),
        &[
            ConversationItem::user("boot"),
            ConversationItem::user("while-on"),
            ConversationItem::user("rebootstrap"),
        ],
        Some(root.path()),
        None,
    )
    .expect("re-enable spawn_capture");
    let n3 = wait_events(&h2, 1).len();
    tee.persist_message(&ConversationItem::user("after-reenable-1"));
    tee.persist_message(&ConversationItem::user("after-reenable-2"));
    tee.flush();
    let n4 = wait_events(&h2, n3 + 2).len();
    assert!(
        n4 >= n3 + 2,
        "Probe B: off→on must not freeze the tee (n3={n3} n4={n4})"
    );
    drop(tee);
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Y2 — timeout fail-open must label status engine native with that reason.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn chunk3a_status_reports_fail_open_reason_after_timeout() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "chunk3a-status-timeout";
    clear_last_serve_outcome(sid);
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let handle_block = handle.clone();
    tokio::task::spawn_blocking(move || {
        let entered = handle_block.block_worker(release_rx);
        let _ = entered.blocking_recv();
    })
    .await
    .unwrap();
    let native = vec![ConversationItem::user("blocked")];
    let decision = serve_request_context(sid, &native, None).await;
    assert!(matches!(
        decision,
        ServeDecision::Native {
            reason: "get_classify_context_timeout"
        }
    ));
    let r = status_report(sid).await;
    assert_eq!(r.context_engine, ContextEngine::Native);
    assert_eq!(r.last_serve_reason, Some("get_classify_context_timeout"));
    let text = format_status_report(&r);
    assert!(
        text.contains("get_classify_context_timeout"),
        "status must surface fail-open reason: {text}"
    );
    assert!(
        !text.contains("Active context engine:** LHC"),
        "must not claim LHC after native fail-open: {text}"
    );
    let _ = release_tx.send(());
    tokio::task::spawn_blocking(move || handle.shutdown_blocking())
        .await
        .unwrap();
    wait_registry_gone(sid);
    clear_last_serve_outcome(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

#[test]
fn chunk3a_off_by_default_noop() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    unsafe { std::env::remove_var("GROK_LHC") };
    assert!(!is_enabled());
    assert!(!capture_active("chunk3a-never"));
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
}

#[test]
fn chunk3a_grok_lhc_on_is_truthy() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    unsafe { std::env::set_var("GROK_LHC", "on") };
    assert!(is_enabled(), "GROK_LHC=on must enable (Y7)");
    let r = resolve_lhc_config(&LhcFileConfig::default());
    assert!(r.enabled.value);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
}

/// Z1 — `/lhc off` (shutdown) must not keep labeling the engine as LHC.
#[test]
fn chunk3a_off_clears_last_serve_engine_label() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "chunk3a-z1-off-clears";
    clear_last_serve_outcome(sid);
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let native = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("hi"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &native, Some(root.path()), None).unwrap();
    let _ = wait_events(&handle, 1);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let decision = rt.block_on(serve_request_context(sid, &native, None));
    if matches!(decision, ServeDecision::Substitute { .. }) {
        let r = rt.block_on(status_report(sid));
        assert_eq!(r.context_engine, ContextEngine::Lhc);
    }
    shutdown_session(sid);
    wait_registry_gone(sid);
    let r_off = rt.block_on(status_report(sid));
    assert_eq!(
        r_off.context_engine,
        ContextEngine::Native,
        "after /lhc off, engine must be native (not stale LHC)"
    );
    assert!(
        last_serve_outcome(sid).is_none(),
        "shutdown must evict last-serve outcome"
    );
    clear_last_serve_outcome(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Z2 — mid-session spawn with sampler registers ModelCall capability.
#[test]
fn chunk3a_mid_session_on_registers_sampler() {
    use grok_lhc_host::LhcInferenceSampler;
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "chunk3a-z2-sampler";
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let mock: Arc<dyn LhcInferenceSampler> = Arc::new(MockLhcInferenceSampler::new());
    let handle = spawn_capture(
        sid,
        Some("/tmp"),
        &[ConversationItem::user("boot")],
        Some(root.path()),
        Some(mock),
    )
    .unwrap();
    assert!(
        inference_sampler_registered(sid),
        "spawn_capture with sampler must register for compact"
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let r = rt.block_on(status_report(sid));
    assert!(r.inference_compact_available);
    let text = format_status_report(&r);
    assert!(
        text.contains("ModelCall compact:** available"),
        "status must report compact available: {text}"
    );
    drop(handle);
    shutdown_session(sid);
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Z3 — stuck worker inspection timeouts must degrade health.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn chunk3a_stuck_worker_health_is_degraded() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "chunk3a-z3-stuck-health";
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let handle_block = handle.clone();
    tokio::task::spawn_blocking(move || {
        let entered = handle_block.block_worker(release_rx);
        let _ = entered.blocking_recv();
    })
    .await
    .unwrap();
    let h = health_check(sid).await;
    assert!(
        !h.ok,
        "stuck worker must report degraded health, notes={:?}",
        h.notes
    );
    assert!(
        h.notes.iter().any(|n| n.contains("timed out")),
        "notes must mention timeout: {:?}",
        h.notes
    );
    let _ = release_tx.send(());
    tokio::task::spawn_blocking(move || handle.shutdown_blocking())
        .await
        .unwrap();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

// ── Schema v5 / G2: shell turn-outcome facts ────────────────────────────

/// Ordering pin: facts attach to the turn they describe (the deferred
/// item-mapped turn_end from terminal Assistant), not the next turn.
/// Mutation: if deferral is removed and facts only stash for the *next*
/// turn_end, turn A's TE stays empty and turn B carries A's outcome.
#[test]
fn g2_turn_end_facts_land_on_described_turn_not_next() {
    let root = TempDir::new().unwrap();
    let sid = "cert-g2-ordering";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();

    handle.persist(&ConversationItem::user("q1"));
    handle.persist(&ConversationItem::assistant("a1"));
    // Do NOT list/flush before facts — that would release the deferred TE empty.
    let facts_a = grok_lhc_host::TurnEndFacts {
        outcome: Some("completed"),
        outcome_reason: Some("completed".into()),
        started_at: Some("2026-07-01T12:00:00.000Z".into()),
        ended_at: Some("2026-07-01T12:00:04.000Z".into()),
    };
    handle.turn_end_facts(1, facts_a);
    handle.flush_blocking();

    let after_a = wait_events(&handle, 3); // user + assistant_text + turn_end
    let te_a = after_a
        .iter()
        .filter_map(|e| {
            e.turn_end_payload()
                .map(|p| (e.idempotency_key().to_string(), p.clone()))
        })
        .collect::<Vec<_>>();
    assert_eq!(te_a.len(), 1, "exactly one turn_end after turn A");
    let (key_a, payload_a) = &te_a[0];
    assert_eq!(
        payload_a.outcome.map(|o| o.as_str()),
        Some("completed"),
        "turn A TE must carry completed — mutation: next-turn stash leaves this empty"
    );
    assert_eq!(payload_a.outcome_reason.as_deref(), Some("completed"));
    assert_eq!(
        payload_a.started_at.as_deref(),
        Some("2026-07-01T12:00:00.000Z")
    );
    assert_eq!(
        payload_a.ended_at.as_deref(),
        Some("2026-07-01T12:00:04.000Z")
    );

    // Turn B: different facts; must not see turn A's outcome.
    handle.persist(&ConversationItem::user("q2"));
    handle.persist(&ConversationItem::assistant("a2"));
    let facts_b = grok_lhc_host::TurnEndFacts {
        outcome: Some("aborted"),
        outcome_reason: Some("cancelled".into()),
        started_at: Some("2026-07-01T12:01:00.000Z".into()),
        ended_at: Some("2026-07-01T12:01:02.000Z".into()),
    };
    handle.turn_end_facts(2, facts_b);
    handle.flush_blocking();

    let after_b = wait_events(&handle, 6);
    let tes: Vec<_> = after_b
        .iter()
        .filter_map(|e| {
            e.turn_end_payload()
                .map(|p| (e.idempotency_key().to_string(), p.clone()))
        })
        .collect();
    assert_eq!(tes.len(), 2, "two turn_ends total");
    let te_b = tes.iter().find(|(k, _)| k != key_a).expect("turn B TE");
    assert_eq!(te_b.1.outcome.map(|o| o.as_str()), Some("aborted"));
    assert_eq!(te_b.1.outcome_reason.as_deref(), Some("cancelled"));
    // Turn A payload unchanged (consume-once; no second event with different key).
    let te_a_again = tes.iter().find(|(k, _)| k == key_a).unwrap();
    assert_eq!(te_a_again.1.outcome.map(|o| o.as_str()), Some("completed"));

    handle.shutdown_blocking();
}

/// Consume-once: a second TurnEndFacts without a new deferred TE emits a
/// shell-authored close (aborted-mid-tools path), not a re-write of the first.
#[test]
fn g2_turn_end_facts_consume_once_deferred() {
    let root = TempDir::new().unwrap();
    let sid = "cert-g2-consume-once";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();

    handle.persist(&ConversationItem::user("q"));
    handle.persist(&ConversationItem::assistant("a"));
    handle.turn_end_facts(
        1,
        grok_lhc_host::TurnEndFacts {
            outcome: Some("completed"),
            outcome_reason: Some("completed".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:01.000Z".into()),
        },
    );
    // Second delivery with no new item TE → shell-authored close.
    handle.turn_end_facts(
        1,
        grok_lhc_host::TurnEndFacts {
            outcome: Some("aborted"),
            outcome_reason: Some("error".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:09.000Z".into()),
        },
    );
    handle.flush_blocking();
    let ev = wait_events(&handle, 4); // user + asst + item TE + shell TE
    let tes: Vec<_> = ev.iter().filter_map(|e| e.turn_end_payload()).collect();
    assert_eq!(tes.len(), 2);
    assert_eq!(tes[0].outcome.map(|o| o.as_str()), Some("completed"));
    assert_eq!(tes[1].outcome.map(|o| o.as_str()), Some("aborted"));
    // Keys must differ (dedup law: facts-bearing never a second event with *same*
    // close identity rewritten — shell key is distinct).
    let keys: Vec<_> = ev
        .iter()
        .filter(|e| e.turn_end_payload().is_some())
        .map(|e| e.idempotency_key().to_string())
        .collect();
    assert_ne!(keys[0], keys[1]);
    handle.shutdown_blocking();
}

/// Replay / replace_history re-map stays empty-facts (scout §4.4).
#[test]
fn g2_replace_history_remap_stays_empty_facts() {
    let root = TempDir::new().unwrap();
    let sid = "cert-g2-replay-empty";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();

    let user = ConversationItem::user("q");
    let asst = ConversationItem::assistant("a");
    handle.persist(&user);
    handle.persist(&asst);
    handle.turn_end_facts(
        1,
        grok_lhc_host::TurnEndFacts {
            outcome: Some("completed"),
            outcome_reason: Some("completed".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:04.000Z".into()),
        },
    );
    handle.flush_blocking();
    let live = wait_events(&handle, 3);
    let live_te = live
        .iter()
        .find_map(|e| e.turn_end_payload())
        .expect("live TE");
    assert_eq!(live_te.outcome.map(|o| o.as_str()), Some("completed"));
    let live_key = live
        .iter()
        .find(|e| e.turn_end_payload().is_some())
        .unwrap()
        .idempotency_key()
        .to_string();

    // Replace with the same items — re-map uses empty facts; dedup keeps live payload.
    handle.replace_history(&[user, asst]);
    handle.flush_blocking();
    thread::sleep(Duration::from_millis(50));
    let after = handle.list_events_blocking().unwrap();
    let te = after
        .iter()
        .find(|e| e.idempotency_key() == live_key)
        .and_then(|e| e.turn_end_payload())
        .expect("survivor TE");
    assert_eq!(
        te.outcome.map(|o| o.as_str()),
        Some("completed"),
        "live facts must survive replace re-map (dedup; empty remap must not win)"
    );
    handle.shutdown_blocking();
}

/// Populated-facts turn_end round-trips through submit_raw (scout §5.2).
#[test]
fn g2_submit_raw_populated_turn_end_facts_round_trip() {
    use lhc::intake_stream::MessageEventInput;
    use serde_json::{Map, json};

    let root = TempDir::new().unwrap();
    let sid = "cert-g2-submit-raw-facts";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();

    let mut te_payload = Map::new();
    te_payload.insert("outcome".into(), json!("aborted"));
    te_payload.insert("outcomeReason".into(), json!("doom_loop_repetition"));
    te_payload.insert("startedAt".into(), json!("2026-07-01T12:00:00.000Z"));
    te_payload.insert("endedAt".into(), json!("2026-07-01T12:00:04.000Z"));

    let events = vec![
        MessageEventInput {
            event_kind: "user_prompt".into(),
            idempotency_key: Some(format!("grok:{sid}:g2raw:u0")),
            actor: "grok".into(),
            harness: "grok-build".into(),
            payload: json!({ "text": "raw turn" }).as_object().cloned().unwrap(),
            extra: Map::new(),
        },
        MessageEventInput {
            event_kind: "assistant_text".into(),
            idempotency_key: Some(format!("grok:{sid}:g2raw:a0")),
            actor: "grok".into(),
            harness: "grok-build".into(),
            payload: json!({ "text": "answer" }).as_object().cloned().unwrap(),
            extra: Map::new(),
        },
        MessageEventInput {
            event_kind: "turn_end".into(),
            idempotency_key: Some(format!("grok:{sid}:g2raw:te0")),
            actor: "grok".into(),
            harness: "grok-build".into(),
            payload: te_payload,
            extra: Map::new(),
        },
    ];
    let batch = handle.submit_raw_blocking(events).expect("submit_raw");
    assert!(
        batch
            .events
            .iter()
            .all(|e| matches!(e.outcome, BatchEventOutcome::Recorded)),
        "{batch:?}"
    );
    handle.flush_blocking();
    let ev = wait_events(&handle, 3);
    let te = ev
        .iter()
        .find_map(|e| e.turn_end_payload())
        .expect("turn_end");
    assert_eq!(te.outcome.map(|o| o.as_str()), Some("aborted"));
    assert_eq!(te.outcome_reason.as_deref(), Some("doom_loop_repetition"));
    assert_eq!(te.started_at.as_deref(), Some("2026-07-01T12:00:00.000Z"));
    assert_eq!(te.ended_at.as_deref(), Some("2026-07-01T12:00:04.000Z"));
    handle.shutdown_blocking();
}

/// Aborted mid-tools (no terminal Assistant) → shell-authored turn_end with facts.
#[test]
fn g2_aborted_without_terminal_assistant_emits_shell_turn_end() {
    let root = TempDir::new().unwrap();
    let sid = "cert-g2-shell-te";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();

    handle.persist(&ConversationItem::user("q"));
    let asst = ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
        content: "calling".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }],
        model_id: None,
        model_fingerprint: None,
        reasoning_effort: None,
    });
    handle.persist(&asst);
    // No terminal toolless assistant — after-turn still delivers facts.
    handle.turn_end_facts(
        3,
        grok_lhc_host::TurnEndFacts {
            outcome: Some("aborted"),
            outcome_reason: Some("cancelled".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:02.000Z".into()),
        },
    );
    handle.flush_blocking();
    // user_prompt + assistant_text + tool_call + shell turn_end
    let ev = wait_events(&handle, 4);
    let te = ev
        .iter()
        .find_map(|e| e.turn_end_payload())
        .expect("shell turn_end");
    assert_eq!(te.outcome.map(|o| o.as_str()), Some("aborted"));
    assert_eq!(te.outcome_reason.as_deref(), Some("cancelled"));
    let te_key = ev
        .iter()
        .find(|e| e.turn_end_payload().is_some())
        .unwrap()
        .idempotency_key();
    assert!(
        te_key.contains("shell_turn_end"),
        "expected shell-authored key, got {te_key}"
    );
    handle.shutdown_blocking();
}

/// Wave B Slice 2 — live capture attaches the same complete identity to
/// reasoning + trailing assistant of one response (usage-independent).
#[test]
fn wave_b_reasoning_and_assistant_share_identity_without_usage() {
    use xai_chat_state::HostAssistantIdentity;
    use xai_grok_sampling_types::synthesized_reasoning_item;

    let root = TempDir::new().unwrap();
    let sid = "cert-wave-b-identity-share";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();

    let identity = HostAssistantIdentity {
        provider: "xai".into(),
        model: Some("grok-4".into()),
        api: Some("responses".into()),
    };
    let mut reasoning = synthesized_reasoning_item("plan");
    reasoning.encrypted_content = Some("enc-shared".into());
    handle.persist_with_capture_facts(
        &ConversationItem::Reasoning(reasoning),
        None,
        Some(identity.clone()),
    );
    handle.persist_with_capture_facts(
        &ConversationItem::assistant("answer"),
        None, // no usage — identity must still land
        Some(identity.clone()),
    );
    handle.flush_blocking();
    // thinking + assistant_text + turn_end
    let ev = wait_events(&handle, 3);

    let thinking = ev
        .iter()
        .find_map(|e| e.assistant_thinking_payload())
        .expect("assistant_thinking");
    assert_eq!(thinking.signature.as_deref(), Some("enc-shared"));
    assert_eq!(thinking.provider.as_deref(), Some("xai"));
    assert_eq!(thinking.model.as_deref(), Some("grok-4"));
    assert_eq!(thinking.api.as_deref(), Some("responses"));

    let text = ev
        .iter()
        .find_map(|e| e.assistant_text_payload())
        .expect("assistant_text");
    assert_eq!(text.provider.as_deref(), Some("xai"));
    assert_eq!(text.model.as_deref(), Some("grok-4"));
    assert_eq!(text.api.as_deref(), Some("responses"));
    assert!(
        text.provider_usage.is_none(),
        "usage must remain independent of identity"
    );

    handle.shutdown_blocking();
}

/// Wave B Slice 2 — consecutive responses with different models keep distinct
/// identity; no cross-response leak of model/api facts.
#[test]
fn wave_b_identity_does_not_leak_across_responses() {
    use xai_chat_state::HostAssistantIdentity;

    let root = TempDir::new().unwrap();
    let sid = "cert-wave-b-identity-no-leak";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();

    let id_a = HostAssistantIdentity {
        provider: "xai".into(),
        model: Some("model-a".into()),
        api: Some("responses".into()),
    };
    let id_b = HostAssistantIdentity {
        provider: "xai".into(),
        model: Some("model-b".into()),
        api: Some("chat_completions".into()),
    };
    handle.persist_with_capture_facts(&ConversationItem::assistant("first"), None, Some(id_a));
    handle.persist_with_capture_facts(&ConversationItem::assistant("second"), None, Some(id_b));
    handle.flush_blocking();
    // 2 * (assistant_text + turn_end) = 4
    let ev = wait_events(&handle, 4);
    let texts: Vec<_> = ev
        .iter()
        .filter_map(|e| e.assistant_text_payload())
        .collect();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0].model.as_deref(), Some("model-a"));
    assert_eq!(texts[0].api.as_deref(), Some("responses"));
    assert_eq!(texts[1].model.as_deref(), Some("model-b"));
    assert_eq!(texts[1].api.as_deref(), Some("chat_completions"));
    handle.shutdown_blocking();
}

/// Wave B Slice 2 — same complete identity re-emits encrypted_content on serve;
/// model mismatch suppresses it while preserving visible reasoning + model_id.
#[test]
fn wave_b_serve_signature_gate_and_model_restore() {
    use lhc::shared_tech::view::{SessionAssistantMessage, SessionAssistantPart};
    use xai_grok_sampling_types::reasoning_item_text;

    let live = grok_lhc_host::LiveRequestIdentity {
        provider: "xai".into(),
        model: Some("grok-4".into()),
        api: Some("responses".into()),
    };
    let matching = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![SessionThreadViewEntry::Message(
            SessionThreadViewMessage::Assistant(SessionAssistantMessage {
                content: vec![
                    SessionAssistantPart {
                        type_: SessionAssistantPartType::Thinking,
                        text: None,
                        thinking: Some("visible plan".into()),
                        thinking_signature: Some("enc-keep".into()),
                        tool_call_id: None,
                        tool_name: None,
                        arguments: None,
                    },
                    SessionAssistantPart {
                        type_: SessionAssistantPartType::Text,
                        text: Some("done".into()),
                        thinking: None,
                        thinking_signature: None,
                        tool_call_id: None,
                        tool_name: None,
                        arguments: None,
                    },
                ],
                source_messages: vec![SessionThreadViewEntrySource {
                    message_id: "a1".into(),
                    idempotency_key: None,
                }],
                provider: Some("xai".into()),
                model: Some("grok-4".into()),
                api: Some("responses".into()),
            }),
        )],
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&matching);
    let items =
        grok_lhc_host::session_view_to_serve_items(&matching, &kinds, Some(&live)).expect("serve");
    match &items[0] {
        ConversationItem::Reasoning(r) => {
            assert_eq!(r.encrypted_content.as_deref(), Some("enc-keep"));
            assert!(reasoning_item_text(r).contains("visible plan"));
        }
        other => panic!("expected Reasoning, got {other:?}"),
    }
    match &items[1] {
        ConversationItem::Assistant(a) => {
            assert_eq!(a.model_id.as_deref(), Some("grok-4"));
            assert_eq!(a.content.as_ref(), "done");
        }
        other => panic!("expected Assistant, got {other:?}"),
    }

    // Model mismatch → suppress signature, keep text + restored stored model.
    let mismatch_live = grok_lhc_host::LiveRequestIdentity {
        provider: "xai".into(),
        model: Some("grok-5".into()),
        api: Some("responses".into()),
    };
    let items2 =
        grok_lhc_host::session_view_to_serve_items(&matching, &kinds, Some(&mismatch_live))
            .expect("serve");
    match &items2[0] {
        ConversationItem::Reasoning(r) => {
            assert_eq!(r.encrypted_content, None);
            assert!(reasoning_item_text(r).contains("visible plan"));
        }
        other => panic!("expected Reasoning without signature, got {other:?}"),
    }
    match &items2[1] {
        ConversationItem::Assistant(a) => {
            assert_eq!(
                a.model_id.as_deref(),
                Some("grok-4"),
                "stored model restored even when signature suppressed"
            );
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
}

/// Wave B race: hook-4 signature admission must use the frozen sampler-attempt
/// backend, not a later live config. Stored identity matches the attempt that
/// will submit; a concurrent API switch after freeze must not suppress a match
/// or admit under the wrong backend.
#[test]
fn wave_b_hook4_signature_uses_frozen_attempt_backend_despite_config_switch() {
    use lhc::shared_tech::view::{SessionAssistantMessage, SessionAssistantPart};
    use xai_grok_sampling_types::reasoning_item_text;

    // Frozen at prepare_sampler_for_turn (FIFO UpdateConfig → Submit).
    let frozen_live = grok_lhc_host::LiveRequestIdentity {
        provider: "xai".into(),
        model: Some("grok-4".into()),
        api: Some("responses".into()),
    };
    // What a post-freeze SetSessionModel would put into live chat-state —
    // must NOT drive admission.
    let post_switch_live = grok_lhc_host::LiveRequestIdentity {
        provider: "xai".into(),
        model: Some("grok-5".into()),
        api: Some("chat_completions".into()),
    };

    let matching = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![SessionThreadViewEntry::Message(
            SessionThreadViewMessage::Assistant(SessionAssistantMessage {
                content: vec![
                    SessionAssistantPart {
                        type_: SessionAssistantPartType::Thinking,
                        text: None,
                        thinking: Some("visible plan".into()),
                        thinking_signature: Some("enc-keep".into()),
                        tool_call_id: None,
                        tool_name: None,
                        arguments: None,
                    },
                    SessionAssistantPart {
                        type_: SessionAssistantPartType::Text,
                        text: Some("done".into()),
                        thinking: None,
                        thinking_signature: None,
                        tool_call_id: None,
                        tool_name: None,
                        arguments: None,
                    },
                ],
                source_messages: vec![SessionThreadViewEntrySource {
                    message_id: "a1".into(),
                    idempotency_key: None,
                }],
                provider: Some("xai".into()),
                model: Some("grok-4".into()),
                api: Some("responses".into()),
            }),
        )],
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&matching);

    // Exact-attempt frozen identity admits encrypted_content.
    let items = grok_lhc_host::session_view_to_serve_items(&matching, &kinds, Some(&frozen_live))
        .expect("serve with frozen attempt");
    match &items[0] {
        ConversationItem::Reasoning(r) => {
            assert_eq!(
                r.encrypted_content.as_deref(),
                Some("enc-keep"),
                "frozen attempt backend must admit signature"
            );
            assert!(reasoning_item_text(r).contains("visible plan"));
        }
        other => panic!("expected Reasoning, got {other:?}"),
    }

    // If hook-4 wrongly read post-switch config, signature would be suppressed.
    let items_wrong =
        grok_lhc_host::session_view_to_serve_items(&matching, &kinds, Some(&post_switch_live))
            .expect("serve with post-switch live");
    match &items_wrong[0] {
        ConversationItem::Reasoning(r) => {
            assert_eq!(
                r.encrypted_content, None,
                "post-switch backend must not admit (proves gate is identity-sensitive)"
            );
            assert!(reasoning_item_text(r).contains("visible plan"));
        }
        other => panic!("expected Reasoning without signature, got {other:?}"),
    }
}

/// Wave B Slice 2 — bootstrap/replace re-map does not invent identity.
#[test]
fn wave_b_replace_history_does_not_invent_identity() {
    use xai_grok_sampling_types::synthesized_reasoning_item;

    let root = TempDir::new().unwrap();
    let sid = "cert-wave-b-replace-no-identity";
    let mut reasoning = synthesized_reasoning_item("old plan");
    reasoning.encrypted_content = Some("enc-hist".into());
    let items = vec![
        ConversationItem::user("q"),
        ConversationItem::Reasoning(reasoning),
        ConversationItem::assistant_with_model("a", "grok-4"),
    ];
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    handle.replace_history(&items);
    handle.flush_blocking();
    let ev = wait_events(&handle, 3);
    let thinking = ev
        .iter()
        .find_map(|e| e.assistant_thinking_payload())
        .expect("thinking");
    assert_eq!(thinking.signature.as_deref(), Some("enc-hist"));
    assert!(
        thinking.provider.is_none() && thinking.model.is_none() && thinking.api.is_none(),
        "replace must not invent provider/model/api"
    );
    let text = ev
        .iter()
        .find_map(|e| e.assistant_text_payload())
        .expect("text");
    assert!(
        text.provider.is_none() && text.model.is_none() && text.api.is_none(),
        "replace must not invent identity on assistant_text"
    );
    handle.shutdown_blocking();
}

// ── Wave B retrieval tools (host half) ───────────────────────────────────

fn rt_block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn seed_two_closed_turns(handle: &CaptureHandle) {
    handle.persist(&ConversationItem::user("what does the config do?"));
    handle.persist(&ConversationItem::assistant("it configures the server"));
    // Terminal assistant without tools closes the turn (item-mapped turn_end).
    handle.persist(&ConversationItem::user("read the file please"));
    handle.persist(&ConversationItem::assistant("here is the file contents"));
    handle.flush_blocking();
    // Wait for at least two turn_end events.
    for _ in 0..200 {
        if let Ok(ev) = handle.list_events_blocking() {
            let turns = ev
                .iter()
                .filter(|e| e.event_kind().as_str() == "turn_end")
                .count();
            if turns >= 2 {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Happy path: exact SDK envelope + impression rows via capture worker.
#[test]
fn wave_b_get_turns_and_messages_happy_path() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "wave-b-happy-pull";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    seed_two_closed_turns(&handle);

    let text = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid, &serde_json::json!({ "ids": ["t1"] }))
            .await
            .expect("get_turns")
    });
    assert!(
        text.starts_with(&lhc::retrieval::format::recall_open("get_turns")),
        "missing envelope open: {text}"
    );
    assert!(
        text.contains(&lhc::retrieval::format::recall_close("get_turns")),
        "missing envelope close"
    );
    assert!(text.contains("<t1>"), "missing turn label: {text}");
    assert!(
        text.contains("what does the config do?"),
        "missing prompt text: {text}"
    );

    // Message ids appear as <mN> tags inside the turn rendering.
    let msg_id = {
        let start = text.find("<m").expect("message tag in turn");
        let rest = &text[start + 1..];
        let end = rest.find('>').expect("close tag");
        rest[..end].to_string()
    };
    assert!(msg_id.starts_with('m'), "expected message id, got {msg_id}");

    let msgs = rt_block_on(async {
        grok_lhc_host::run_get_messages(sid, &serde_json::json!({ "ids": [msg_id] }))
            .await
            .expect("get_messages")
    });
    assert!(msgs.starts_with(&lhc::retrieval::format::recall_open("get_messages")));
    assert!(msgs.contains("what does the config do?"));

    let imps = rt_block_on(async { handle.list_impressions().await.expect("imps") });
    assert!(
        imps.len() >= 2,
        "happy-path pulls must write impression rows; got {}",
        imps.len()
    );

    handle.shutdown_blocking();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Invalid args must not call the SDK (zero impressions).
#[test]
fn wave_b_invalid_args_zero_impressions() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "wave-b-val-zero-imp";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    seed_two_closed_turns(&handle);
    let before = rt_block_on(async { handle.list_impressions().await.unwrap().len() });
    assert_eq!(before, 0);

    let cases = [
        serde_json::json!({ "ids": ["m1"] }),
        serde_json::json!({ "ids": [] }),
        serde_json::json!({ "ids": ["t1"], "from": -1 }),
        serde_json::json!({ "ids": ["t1"], "from": null }),
        serde_json::json!({ "ids": ["t1"], "budget": 4000 }),
    ];
    for args in cases {
        let err = rt_block_on(async { grok_lhc_host::run_get_turns(sid, &args).await });
        assert!(err.is_err(), "expected refuse for {args}");
    }

    let after = rt_block_on(async { handle.list_impressions().await.unwrap().len() });
    assert_eq!(after, 0, "validation failures must create zero impressions");

    handle.shutdown_blocking();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Inactive session fails explicitly; never opens another session's DB.
#[test]
fn wave_b_inactive_and_cross_session_isolation() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid_a = "wave-b-iso-a";
    let sid_b = "wave-b-iso-b";
    let ha = spawn_capture(sid_a, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    seed_two_closed_turns(&ha);
    let hb = spawn_capture(sid_b, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    hb.persist(&ConversationItem::user("only in b"));
    hb.persist(&ConversationItem::assistant("b answer"));
    hb.flush_blocking();

    // A cannot see B's turns by using B's session id with A's handle — tools
    // resolve by the bound session_id only.
    let a_text = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid_a, &serde_json::json!({ "ids": ["t1"] }))
            .await
            .unwrap()
    });
    assert!(a_text.contains("what does the config do?"));
    assert!(!a_text.contains("only in b"));

    // Inactive id refuses without touching A or B.
    let err = rt_block_on(async {
        grok_lhc_host::run_get_turns("wave-b-never-opened", &serde_json::json!({ "ids": ["t1"] }))
            .await
    });
    assert!(
        err.unwrap_err().contains("not active"),
        "expected inactive error"
    );

    // After A shuts down, A's tools must fail even though B is live.
    ha.shutdown_blocking();
    wait_registry_gone(sid_a);
    let err = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid_a, &serde_json::json!({ "ids": ["t1"] })).await
    });
    assert!(err.unwrap_err().contains("not active"));

    // B still works.
    let b_text = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid_b, &serde_json::json!({ "ids": ["t1"] }))
            .await
            .unwrap()
    });
    assert!(b_text.contains("only in b") || b_text.contains("b answer") || b_text.contains("<t1>"));

    hb.shutdown_blocking();
    wait_registry_gone(sid_b);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Continuation from `from` > 0 on an oversized turn.
#[test]
fn wave_b_continuation_from_nonzero() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "wave-b-slice-from";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let big_body: String = (0..2000)
        .map(|i| {
            format!(
                "line {i} of the very long log with filler words for token weight \
                 and more padding text so the budget walk must slice"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    handle.persist(&ConversationItem::user("dump the log please"));
    handle.persist(&ConversationItem::assistant(format!(
        "full log follows\n{big_body}"
    )));
    handle.flush_blocking();
    // Wait for turn close.
    wait_events(&handle, 3);

    let first = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid, &serde_json::json!({ "ids": ["t1"] }))
            .await
            .expect("first slice")
    });
    assert!(
        first.contains("served tok 0–") || first.contains("served tok 0-"),
        "expected head slice receipt: {first}"
    );
    assert!(
        first.contains("Next slice: get_turns({\"ids\":[\"t1\"],\"from\":"),
        "expected continuation instruction: {first}"
    );

    let second = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid, &serde_json::json!({ "ids": ["t1"], "from": 8000 }))
            .await
            .expect("continuation")
    });
    assert!(
        second.contains("served tok 8000–")
            || second.contains("served tok 8000-")
            || second.contains("nothing at token offset 8000"),
        "expected continuation window: {second}"
    );

    handle.shutdown_blocking();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Dedupe before 32-id cap: 38 copies of one id is one unique (serves), not refuse.
#[test]
fn wave_b_dedupe_before_32_cap() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "wave-b-dedupe-cap";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    seed_two_closed_turns(&handle);

    let ids: Vec<String> = std::iter::repeat_n("t1".to_string(), 38).collect();
    let text = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid, &serde_json::json!({ "ids": ids }))
            .await
            .expect("38 copies of t1 must budget as one id")
    });
    assert!(text.contains("<t1>") || text.contains("what does the config"));

    // 33 unique → host refuse, zero impressions.
    let before = rt_block_on(async { handle.list_impressions().await.unwrap().len() });
    let ids33: Vec<String> = (1..=33).map(|i| format!("t{i}")).collect();
    let err = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid, &serde_json::json!({ "ids": ids33 })).await
    });
    assert!(err.unwrap_err().contains("too many ids"));
    let after = rt_block_on(async { handle.list_impressions().await.unwrap().len() });
    assert_eq!(after, before, "over-cap refuse must not add impressions");

    handle.shutdown_blocking();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Retrieval is serialized on the capture worker with persist (no independent DB).
#[test]
fn wave_b_retrieval_serialized_with_capture() {
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "wave-b-serial-worker";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    seed_two_closed_turns(&handle);

    // Block the worker, then queue a persist and a get_turns — both complete
    // after release, proving they share the same queue owner.
    let (release_tx, release_rx) = oneshot::channel();
    let entered_rx = handle.block_worker(release_rx);
    // Wait until worker is blocked.
    let _ = entered_rx.blocking_recv();

    handle.persist(&ConversationItem::user("while-blocked"));
    let handle2 = handle.clone();
    let pull = thread::spawn(move || {
        rt_block_on(async {
            grok_lhc_host::run_get_turns(
                handle2.session_id(),
                &serde_json::json!({ "ids": ["t1"] }),
            )
            .await
        })
    });

    // Give the pull a moment to queue behind the block.
    thread::sleep(Duration::from_millis(50));
    let _ = release_tx.send(());
    let text = pull.join().unwrap().expect("get_turns after unblock");
    assert!(text.contains("<t1>") || text.contains("what does the config"));

    handle.flush_blocking();
    let ev = wait_events(&handle, 1);
    let has_blocked_user = ev.iter().any(|e| {
        e.event_kind().as_str() == "user_prompt"
            && e.text_payload()
                .is_some_and(|p| p.text.contains("while-blocked"))
    });
    assert!(
        has_blocked_user,
        "persist while blocked must still land after release"
    );

    handle.shutdown_blocking();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Delayed open: registry presence is provisional; tools/retrieval must not
/// treat Pending as ready; wait_until_open succeeds only after release.
#[test]
fn wave_b_delayed_open_not_ready_until_archive_opens() {
    let _g = env_lock();
    clear_open_hold_for_test();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "wave-b-delayed-open";
    let (release_tx, release_rx) = oneshot::channel();
    set_open_hold_for_test(release_rx);

    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    assert!(
        capture_active(sid),
        "pre-open registry design: capture_active while open held"
    );
    assert!(
        !capture_archive_ready(sid),
        "archive must not be ready while open is held"
    );
    assert_eq!(handle.open_state(), CaptureOpenState::Pending);

    // Retrieval must fail explicitly (not hang on the worker queue).
    let err = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid, &serde_json::json!({ "ids": ["t1"] })).await
    });
    assert!(
        err.as_ref().is_err_and(|e| e.contains("not ready")),
        "pending open must fail retrieval explicitly: {err:?}"
    );

    // Bounded wait while still pending → TimedOut (do not hang forever).
    let timed = rt_block_on(async { handle.wait_until_open(Duration::from_millis(80)).await });
    assert_eq!(timed, Err(CaptureOpenWaitError::TimedOut));

    // Release open; readiness becomes true.
    let _ = release_tx.send(());
    rt_block_on(async {
        wait_capture_archive_ready(sid, Duration::from_secs(5))
            .await
            .expect("open after release");
    });
    assert!(capture_archive_ready(sid));
    assert_eq!(handle.open_state(), CaptureOpenState::Ready);

    handle.shutdown_blocking();
    wait_registry_gone(sid);
    clear_open_hold_for_test();
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Refused open: wait_until_open reports Failed; capture leaves registry;
/// retrieval stays inactive (no hang).
#[test]
fn wave_b_refused_open_reports_failed_and_clears_registry() {
    let _g = env_lock();
    clear_open_hold_for_test();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "wave-b-refused-open";
    // Legitimate session then orphan the thread (registry wiped) → open refuses.
    {
        let h = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
        h.persist(&ConversationItem::user("x"));
        h.flush_blocking();
        let _ = wait_exact(&h, 1);
        h.shutdown_blocking();
        wait_registry_gone(sid);
    }
    let registry = root.path().join("registry.sqlite");
    std::fs::remove_file(&registry).unwrap();
    let _ = std::fs::remove_file(root.path().join("registry.sqlite-wal"));
    let _ = std::fs::remove_file(root.path().join("registry.sqlite-shm"));
    assert!(thread_file_path(root.path(), sid).exists());

    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    let open_err = rt_block_on(async { handle.wait_until_open(Duration::from_secs(5)).await });
    assert_eq!(
        open_err,
        Err(CaptureOpenWaitError::Failed),
        "refused open must surface Failed, not hang"
    );
    wait_registry_gone(sid);
    assert!(!capture_active(sid));
    assert!(!capture_archive_ready(sid));

    let pull = rt_block_on(async {
        grok_lhc_host::run_get_turns(sid, &serde_json::json!({ "ids": ["t1"] })).await
    });
    assert!(
        pull.is_err(),
        "refused session must not serve retrieval: {pull:?}"
    );

    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}

/// Successful open path still advertises ready for tool publication consumers.
#[test]
fn wave_b_spawn_becomes_archive_ready() {
    let _g = env_lock();
    clear_open_hold_for_test();
    let root = TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid = "wave-b-ready-ok";
    let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    rt_block_on(async {
        handle
            .wait_until_open(Duration::from_secs(5))
            .await
            .expect("normal open");
    });
    assert!(capture_archive_ready(sid));
    handle.shutdown_blocking();
    wait_registry_gone(sid);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
}
