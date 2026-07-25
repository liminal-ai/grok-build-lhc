//! Chunk 1 certification — exact counts and key-set equality.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use grok_lhc_host::{
    CaptureHandle, CompactMode, MockLhcInferenceSampler, ServeDecision, apply_serve_decision,
    body_has_tool_cycle, capture_active, capture_model_or_thinking_change, decide_substitution,
    encode_session_id_for_path, is_enabled, lookup_session, paths_disagree, resolve_compact_mode,
    serve_request_context, set_compact_mode_for_test, shutdown_session, spawn_capture,
    tee_chat_persistence, thread_file_path,
};
use lhc::intake_stream::{BatchEventOutcome, BatchSkipReason, EventRecord};
use tempfile::TempDir;
use tokio::sync::oneshot;
use xai_chat_state::{NullChatPersistence, PersistenceRecord};
use xai_grok_sampling_types::{ConversationItem, SyntheticReason, ToolCall};

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

    // Discriminating observation (D2): empty bootstrap must see 0 events —
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
    let (mapped, _) = grok_lhc_host::map_history(sid, 0, std::slice::from_ref(&item));
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
    // Spawn must not block; refused open unregisters asynchronously (C1/B5).
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

    // Spawn must not block; B2 refuse is observable via unregister (C1).
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

    // Spawn must not block; B5 refuse is observable via unregister (C1).
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

// ── Chunk 2: inference / serving / compact / watchdog ─────────────────

/// Mock sampler is injectable at spawn (hook-2 argument widen) and satisfies
/// the ModelCall contract (text + provenance model + request_messages).
#[tokio::test(flavor = "multi_thread")]
async fn chunk2_mock_sampler_registered_at_spawn() {
    use grok_lhc_host::{LhcInferenceOp, LhcInferenceSampler};
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
    let sample = mock
        .sample(LhcInferenceOp::SmoothPrompt, "ping".into())
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
}

/// Serving fails open when capture is inactive.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn chunk2_serve_fail_open_when_inactive() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    unsafe { std::env::remove_var("GROK_LHC") };
    let native = vec![ConversationItem::user("keep-me")];
    let decision = serve_request_context("no-such-session", native.clone()).await;
    let (items, substituted) = apply_serve_decision(native, decision);
    assert!(!substituted);
    assert_eq!(items.len(), 1);
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
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
    let decision = serve_request_context(sid, native.clone()).await;
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

/// Mid-tool-cycle native body is replaced wholesale (never hybrid).
#[test]
fn chunk2_mid_tool_cycle_all_lhc_or_native() {
    use lhc::shared_tech::view::{
        LlmRequestContext, LlmRequestContextMessage, LlmRequestContextPart,
        LlmRequestContextPartType, LlmRequestContextRole,
    };
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
    let ctx = LlmRequestContext {
        thread_id: "t".into(),
        messages: vec![LlmRequestContextMessage {
            role: LlmRequestContextRole::User,
            content: vec![LlmRequestContextPart {
                type_: LlmRequestContextPartType::Text,
                text: "run".into(),
            }],
        }],
    };
    match decide_substitution(&native, &ctx) {
        ServeDecision::Substitute { items } => {
            assert!(!body_has_tool_cycle(&items));
            assert!(matches!(&items[0], ConversationItem::System(_)));
        }
        ServeDecision::Native { reason } => panic!("expected all-LHC substitute, got {reason}"),
    }
}

/// Out-of-runtime watchdog: if the async guard body hangs synchronously,
/// a peer thread trips before the tokio timeout can (closes Chunk 1 #2).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn chunk2_async_guard_out_of_runtime_watchdog() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let _g = env_lock();
    let root = TempDir::new().unwrap();
    let sid = "cert-chunk2-watchdog";
    let done = Arc::new(AtomicBool::new(false));
    let done_w = Arc::clone(&done);
    let watchdog = thread::spawn(move || {
        for _ in 0..100 {
            if done_w.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("out-of-runtime watchdog: async tee guard hung (>5s)");
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        with_lhc_env(root.path(), || {
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
    })
    .await
    .expect("tee install blocked the current-thread runtime");
    done.store(true, Ordering::SeqCst);
    watchdog.join().expect("watchdog thread");
}
