//! Chunk 3B harness track — mechanism coverage through the host path.
//!
//! Uses `create_deterministic_inference_callbacks` for **plumbing** only
//! (write-back, idempotency, crash windows, kind conservation). Derivation
//! **content** is certified against real `grok-4.5` @ low thinking in
//! `xai-grok-shell::…::lhc_real_inference_g2` (Lee's unit-19 ruling).

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use grok_lhc_host::{
    CaptureHandle, CompactMode, SourceKindIndex, apply_serve_decision,
    build_writeback_conversation, capture_active, capture_model_or_thinking_change,
    compare_serve_equivalence, decide_substitution, equivalence_snapshot, is_enabled,
    lookup_session, observe_serve_equivalence, project_conversation_canonical, replace_call_count,
    replace_compact_for_writeback, reset_compact_call_counters, reset_equivalence_counters,
    run_five_gates_on_body, set_compact_mode_for_test, set_compact_params_override_for_test,
    set_use_deterministic_inference_for_test, spawn_capture, tee_chat_persistence,
    thread_file_path,
};
use lhc::intake_stream::EventRecord;
use lhc::shared_tech::view::{
    PartialViewProfilePercentages, SessionAssistantMessage, SessionAssistantPart,
    SessionAssistantPartType, SessionModelChangeEntry, SessionThreadView, SessionThreadViewEntry,
    SessionThreadViewEntrySource, SessionThreadViewMessage, SessionThreadViewRuntimeEntry,
    SessionToolResultMessage, SessionUserMessage, ViewCompactParams,
};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use xai_chat_state::{ChatStateActor, MockChatPersistence};
use xai_grok_sampling_types::{ConversationItem, SamplingConfig, ToolCall};

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn sampling_config() -> SamplingConfig {
    SamplingConfig {
        base_url: "https://api.example.com".into(),
        model: "test-model".into(),
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        api_backend: Default::default(),
        extra_headers: Default::default(),
        query_params: Default::default(),
        env_http_headers: Default::default(),
        context_window: NonZeroU64::new(128_000).unwrap(),
        reasoning_effort: None,
        stream_tool_calls: None,
    }
}

fn wait_events(handle: &CaptureHandle, min: usize) -> Vec<EventRecord> {
    for _ in 0..240 {
        match handle.list_events_blocking() {
            Ok(ev) if ev.len() >= min => return ev,
            Ok(_) | Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
    handle
        .list_events_blocking()
        .expect("list_events after wait")
}

async fn wait_events_async(handle: &CaptureHandle, min: usize) -> Vec<EventRecord> {
    for _ in 0..240 {
        match handle.list_events().await {
            Ok(ev) if ev.len() >= min => return ev,
            Ok(_) | Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    handle.list_events().await.expect("list_events after wait")
}

fn wait_registry_gone(session_id: &str) {
    for _ in 0..120 {
        if !capture_active(session_id) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("registry entry still present for {session_id}");
}

fn keys(events: &[EventRecord]) -> BTreeSet<String> {
    events
        .iter()
        .map(|e| e.idempotency_key().to_string())
        .collect()
}

fn tight_compact_params() -> ViewCompactParams {
    ViewCompactParams {
        lower_bound: Some(400.0),
        percentages: Some(PartialViewProfilePercentages {
            full: Some(25.0),
            smooth: Some(25.0),
            detailed: Some(25.0),
            brief: Some(25.0),
        }),
    }
}

fn multi_turn_native(system: &str, turns: usize) -> Vec<ConversationItem> {
    let blob = "word ".repeat(200);
    let mut items = vec![ConversationItem::system(system)];
    for t in 0..turns {
        let mut u = ConversationItem::user(format!("turn {t} {blob}"));
        u.set_prompt_index(t);
        items.push(u);
        if t == 1 {
            items.push(ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: format!("c{t}").into(),
                name: "bash".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }]));
            items.push(ConversationItem::tool_result(
                format!("c{t}"),
                "file_a\nfile_b",
            ));
        }
        items.push(ConversationItem::assistant(format!("answer {t} {blob}")));
    }
    items
}

// ── Adapter fixture helpers (mirror certification writeback_fixture) ───

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

fn adapter_simulated_writeback_body() -> (Vec<ConversationItem>, Vec<ConversationItem>) {
    let mut u0 = ConversationItem::user("old-0");
    u0.set_prompt_index(0);
    let mut u1 = ConversationItem::user("please investigate area 9");
    u1.set_prompt_index(1);
    let mut u2 = ConversationItem::user("live-2");
    u2.set_prompt_index(2);
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
    let view = SessionThreadView {
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
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&view)
        .with("rn1", lhc::messages::MessageKind::RuntimeNote);
    let body = build_writeback_conversation(&native, &view, &kinds).expect("fixture body");
    (native, body)
}

fn body_fingerprint(items: &[ConversationItem]) -> String {
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
            let prompt = match i {
                ConversationItem::User(u) => format!("{:?}", u.prompt_index),
                _ => "None".into(),
            };
            let text = i.text_content();
            let text_short = if text.len() > 96 {
                format!("{}…", &text[..96])
            } else {
                text
            };
            format!("{kind}:{prompt}:{tools}:{text_short}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn band_count(view: &SessionThreadView) -> usize {
    view.entries
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

fn restore_env(
    prev: Option<std::ffi::OsString>,
    prev_root: Option<std::ffi::OsString>,
    prev_c: Option<std::ffi::OsString>,
    prev_e: Option<std::ffi::OsString>,
) {
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
    match prev_c {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_COMPACT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_COMPACT") },
    }
    match prev_e {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_COMPACT_EXPERIMENTAL", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_COMPACT_EXPERIMENTAL") },
    }
    set_use_deterministic_inference_for_test(false);
    set_compact_mode_for_test(None);
    set_compact_params_override_for_test(None);
}

struct HarnessSession {
    sid: String,
    root: TempDir,
    actor: xai_chat_state::ChatStateHandle,
    capture: CaptureHandle,
    rt: tokio::runtime::Runtime,
    _event_rx: mpsc::UnboundedReceiver<xai_chat_state::ChatStateEvent>,
    _cancel: CancellationToken,
}

impl HarnessSession {
    /// Owns a multi-thread runtime so ChatStateActor can spawn and so capture
    /// RPCs use async send (never `blocking_*` on a worker thread).
    fn open_replace(sid: &str, native: Vec<ConversationItem>) -> Self {
        let root = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("GROK_LHC", "1");
            std::env::set_var("GROK_LHC_ROOT", root.path());
            std::env::set_var("GROK_LHC_COMPACT", "replace");
            std::env::set_var("GROK_LHC_COMPACT_EXPERIMENTAL", "1");
        }
        set_use_deterministic_inference_for_test(true);
        set_compact_mode_for_test(Some(CompactMode::Replace));

        let rt = rt();
        let (mock, _rx) = MockChatPersistence::new();
        let tee = tee_chat_persistence(sid, "/tmp", &native, Box::new(mock), None);
        assert!(
            capture_active(sid),
            "tee must register capture when GROK_LHC=1"
        );
        let capture = lookup_session(sid).expect("capture handle");
        rt.block_on(wait_events_async(&capture, 1));

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let actor = {
            let _enter = rt.enter();
            ChatStateActor::spawn(native, sampling_config(), tee, event_tx, cancel.clone())
        };
        // Intake + derivation settle before compact.
        rt.block_on(async {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if let Ok(ev) = capture.list_events().await
                    && ev.len() >= 10
                {
                    break;
                }
            }
        });
        Self {
            sid: sid.to_string(),
            root,
            actor,
            capture,
            rt,
            _event_rx: event_rx,
            _cancel: cancel,
        }
    }

    fn run_replace_writeback(&self) -> Vec<ConversationItem> {
        set_compact_params_override_for_test(Some(tight_compact_params()));
        reset_compact_call_counters();
        let sid = self.sid.clone();
        let actor = self.actor.clone();
        self.rt.block_on(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let wb = replace_compact_for_writeback(&sid)
                .await
                .expect("replace_compact_for_writeback");
            assert_eq!(replace_call_count(), 1);
            let bands = band_count(&wb.view);
            eprintln!(
                "3B harness: view entries={} bands={bands} receipt={}",
                wb.view.entries.len(),
                wb.receipt_total_tokens
            );
            assert!(
                bands > 0,
                "harness compact must emit typed bands (got 0); tight params + deterministic cbs required"
            );
            let native_before = actor.get_conversation().await;
            let body = build_writeback_conversation(&native_before, &wb.view, &wb.kinds)
                .expect("build_writeback_conversation");
            actor.replace_conversation_for_compaction(body.clone());
            let after = actor.get_conversation().await;
            assert_eq!(
                body_fingerprint(&after),
                body_fingerprint(&body),
                "ChatStateActor must hold the write-back body after replace"
            );
            body
        })
    }

    fn shutdown(self) {
        set_compact_params_override_for_test(None);
        set_compact_mode_for_test(None);
        set_use_deterministic_inference_for_test(false);
        // Shutdown off the runtime: join the worker thread from a plain OS thread.
        let capture = self.capture;
        let sid = self.sid.clone();
        drop(self.rt);
        capture.shutdown_blocking();
        wait_registry_gone(&sid);
        drop(self.root);
    }
}

// ── B1 / G2 ─────────────────────────────────────────────────────────────

#[test]
fn b1_g2_real_writeback_body_vs_fixture_and_gates() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let prev_c = std::env::var_os("GROK_LHC_COMPACT");
    let prev_e = std::env::var_os("GROK_LHC_COMPACT_EXPERIMENTAL");

    let sid = "harness-b1-g2";
    let native = multi_turn_native("sys", 12);
    let h = HarnessSession::open_replace(sid, native.clone());
    let real_body = h.run_replace_writeback();

    let (_fix_native, fixture_body) = adapter_simulated_writeback_body();
    let real_fp = body_fingerprint(&real_body);
    let fixture_fp = body_fingerprint(&fixture_body);
    let matched = real_fp == fixture_fp;
    eprintln!(
        "=== G2 real write-back body ({} items) ===\n{real_fp}",
        real_body.len()
    );
    eprintln!(
        "=== G2 adapter fixture body ({} items) ===\n{fixture_fp}",
        fixture_body.len()
    );
    if matched {
        eprintln!("G2: real body MATCHES adapter fixture fingerprint");
    } else {
        eprintln!(
            "G2: real body DIFFERS from adapter fixture — gate subject = real body\n\
             FINDING (G2): Chunk 2 hard-gate fixtures do not match harness Replace body."
        );
    }

    run_five_gates_on_body(
        sid,
        &native,
        &real_body,
        h.root.path(),
        "deterministic-harness-body",
    );

    // B8.3 calibration against deterministic harness body
    reset_equivalence_counters();
    let cal = compare_serve_equivalence(&real_body, &real_body);
    assert!(!cal.structural_divergence && !cal.informational_divergence);
    let _ = project_conversation_canonical(&real_body);
    let obs = observe_serve_equivalence(sid, Some(0), true, true, &real_body, &real_body);
    assert!(
        obs.compared,
        "B8.3/B8.1: calibration must produce a compared turn"
    );
    let snap = equivalence_snapshot();
    eprintln!(
        "B8.3 calibration (deterministic-harness-body): compared={} fallen_back={} \
         structural={} informational={} ratio={}:{}",
        snap.turns_served_and_compared,
        snap.turns_fallen_back,
        snap.structural_divergences,
        snap.informational_divergences,
        snap.turns_fallen_back,
        snap.turns_served_and_compared
    );
    assert!(snap.turns_served_and_compared > 0);

    h.shutdown();
    restore_env(prev, prev_root, prev_c, prev_e);
}

// ── B3 /btw + memory-flush input (ruling premise) ───────────────────────

/// B3 — `/btw` and memory flush read native conversation. After Replace
/// write-back that native body **is** the LHC-compacted body. This test
/// proves the input those consumers see. Full model-backed `/btw` /
/// `run_memory_flush` require a sampling client → live track.
#[test]
fn b3_btw_and_memory_read_lhc_compacted_native_body() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let prev_c = std::env::var_os("GROK_LHC_COMPACT");
    let prev_e = std::env::var_os("GROK_LHC_COMPACT_EXPERIMENTAL");

    let sid = "harness-b3-consumers";
    let native = multi_turn_native("sys", 12);
    let h = HarnessSession::open_replace(sid, native);
    let body = h.run_replace_writeback();
    let conv = h.rt.block_on(h.actor.get_conversation());
    assert_eq!(
        body_fingerprint(&conv),
        body_fingerprint(&body),
        "B3: get_conversation (what /btw + memory flush read) must be the LHC write-back body"
    );
    assert!(
        conv.iter().any(|i| i.text_content().contains("[context")),
        "B3: compacted body must carry a context band for consumers"
    );
    eprintln!(
        "B3: ruling premise HOLD — native conversation after write-back is the LHC body \
         ({} items, band present). Full /btw model call + memory_dream flush → live track.",
        conv.len()
    );
    h.shutdown();
    restore_env(prev, prev_root, prev_c, prev_e);
}

// ── B2 session-id coupling ──────────────────────────────────────────────

#[test]
fn b2_hook2_hook3_session_id_coupling_no_cross_leak() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let root = TempDir::new().unwrap();
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    set_use_deterministic_inference_for_test(true);

    let a = "harness-b2-a";
    let b = "harness-b2-b";
    let seed = vec![ConversationItem::user("seed")];
    let ha = spawn_capture(a, Some("/tmp"), &seed, Some(root.path()), None).unwrap();
    let hb = spawn_capture(b, Some("/tmp"), &seed, Some(root.path()), None).unwrap();
    let _ = wait_events(&ha, 1);
    let _ = wait_events(&hb, 1);

    capture_model_or_thinking_change(a, "m-a", "m-a2", None, Some("high"));
    ha.flush_blocking();
    let ev_a = wait_events(&ha, 2);
    assert!(
        ev_a.iter().any(|e| e.model_change_payload().is_some()),
        "model change must land in session A"
    );

    capture_model_or_thinking_change("harness-b2-WRONG", "x", "y", None, Some("high"));
    thread::sleep(Duration::from_millis(100));
    let ev_b = hb.list_events_blocking().unwrap();
    assert!(
        ev_b.iter().all(|e| e.model_change_payload().is_none()),
        "wrong id must not write into B"
    );

    let keys_a = keys(&ha.list_events_blocking().unwrap());
    ha.shutdown_blocking();
    wait_registry_gone(a);
    let ha2 = spawn_capture(a, Some("/tmp"), &[], Some(root.path()), None).unwrap();
    capture_model_or_thinking_change(a, "m-a2", "m-a3", Some("high"), Some("none"));
    ha2.flush_blocking();
    let ev_resume = wait_events(&ha2, keys_a.len() + 1);
    assert!(
        ev_resume.iter().any(|e| {
            e.model_change_payload()
                .is_some_and(|p| p.new_model.contains("m-a3"))
        }),
        "resume: model change must append on reopened A"
    );

    let fork = "harness-b2-fork";
    let hf = spawn_capture(fork, Some("/tmp"), &seed, Some(root.path()), None).unwrap();
    let _ = wait_events(&hf, 1);
    capture_model_or_thinking_change(fork, "mf", "mf2", None, None);
    hf.flush_blocking();
    let ev_f = wait_events(&hf, 2);
    assert_ne!(
        thread_file_path(root.path(), a),
        thread_file_path(root.path(), fork)
    );
    assert!(ev_f.iter().any(|e| e.model_change_payload().is_some()));

    ha2.shutdown_blocking();
    hb.shutdown_blocking();
    hf.shutdown_blocking();
    wait_registry_gone(a);
    wait_registry_gone(b);
    wait_registry_gone(fork);
    set_use_deterministic_inference_for_test(false);
    restore_env(prev, prev_root, None, None);
}

// ── B2 silent-block ─────────────────────────────────────────────────────

#[test]
fn b2_silent_block_detected_by_out_of_thread_watchdog() {
    let _g = env_lock();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let (block_tx, block_rx) = std::sync::mpsc::channel::<()>();
    thread::spawn(move || {
        let _ = block_rx.recv_timeout(Duration::from_secs(60));
        let _ = done_tx.send(());
    });
    let timed_out = done_rx.recv_timeout(Duration::from_millis(200)).is_err();
    assert!(
        timed_out,
        "watchdog must observe silent block within budget"
    );
    let _ = block_tx.send(());
    let _ = done_rx.recv_timeout(Duration::from_secs(2));
    eprintln!(
        "B2 silent-block: harness CAN detect out-of-thread hangs via recv_timeout; \
         CANNOT detect in-task async stalls without an external controller. \
         Open item narrowed for in-runtime silent awaits — live/CI still owns that."
    );
}

// ── B4 crash window (adapter half via harness body) ─────────────────────

#[test]
fn b4_lhc_ahead_of_native_replace_is_transient_on_harness_body() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let prev_c = std::env::var_os("GROK_LHC_COMPACT");
    let prev_e = std::env::var_os("GROK_LHC_COMPACT_EXPERIMENTAL");

    let sid = "harness-b4-ahead";
    let native = multi_turn_native("sys", 12);
    let h = HarnessSession::open_replace(sid, native.clone());
    // Compact LHC but do NOT write native yet — LHC ahead.
    set_compact_params_override_for_test(Some(tight_compact_params()));
    let wb =
        h.rt.block_on(replace_compact_for_writeback(sid))
            .expect("compact");
    let body = build_writeback_conversation(&native, &wb.view, &wb.kinds).unwrap();
    let needle = body
        .iter()
        .find_map(|i| {
            let t = i.text_content();
            t.contains("[context").then_some(t)
        })
        .unwrap_or_else(|| body[0].text_content());

    // Apply write-back onto capture while actor still holds old native.
    h.capture.replace_history(&body);
    h.capture.flush_blocking();
    let once = wait_events(&h.capture, 1);
    let once_keys = keys(&once);
    h.capture.replace_history(&body);
    h.capture.flush_blocking();
    thread::sleep(Duration::from_millis(150));
    let again = h.capture.list_events_blocking().unwrap();
    assert_eq!(keys(&again), once_keys, "retry must not re-key");
    let hits = again
        .iter()
        .filter(|e| e.text_payload().is_some_and(|p| p.text.contains(&needle)))
        .count();
    assert_eq!(hits, 1, "band/summary must not double-record");

    // Evidence checklist
    assert!(
        !body
            .iter()
            .any(|i| matches!(i, ConversationItem::ToolResult(_))
                && !body.iter().any(
                    |j| matches!(j, ConversationItem::Assistant(a) if !a.tool_calls.is_empty())
                )),
        "no dangling tool results without tool calls in body shape check skipped if no tools"
    );
    eprintln!(
        "B4 evidence: no_duplicate_keys=true events={} dangling_tool_check=structural \
         sqlite_exists={}",
        again.len(),
        thread_file_path(h.root.path(), sid).exists()
    );

    h.shutdown();
    restore_env(prev, prev_root, prev_c, prev_e);
}

// ── B5 rollback ─────────────────────────────────────────────────────────

#[test]
fn b5_rollback_disable_after_writeback_continues_native() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let prev_c = std::env::var_os("GROK_LHC_COMPACT");
    let prev_e = std::env::var_os("GROK_LHC_COMPACT_EXPERIMENTAL");

    let sid = "harness-b5-rollback";
    let native = multi_turn_native("sys", 12);
    let h = HarnessSession::open_replace(sid, native);
    let body = h.run_replace_writeback();
    assert!(matches!(body.first(), Some(ConversationItem::System(_))));

    unsafe { std::env::remove_var("GROK_LHC") };
    assert!(!is_enabled());
    h.capture.shutdown_blocking();
    wait_registry_gone(sid);

    let conv = h.rt.block_on(h.actor.get_conversation());
    assert_eq!(body_fingerprint(&conv), body_fingerprint(&body));
    h.actor
        .push_user_message(ConversationItem::user("after-disable"));
    let after = h.rt.block_on(h.actor.get_conversation());
    assert!(
        after
            .iter()
            .any(|i| i.text_content().contains("after-disable"))
    );

    let thread = thread_file_path(h.root.path(), sid);
    assert!(thread.exists(), "LHC sqlite must survive disable");
    // Direct reopen recovers event log (product /lhc on path).
    unsafe { std::env::set_var("GROK_LHC", "1") };
    let cap = spawn_capture(sid, Some("/tmp"), &[], Some(h.root.path()), None).unwrap();
    let ev = cap.list_events_blocking().unwrap();
    eprintln!("B5 recoverability: LHC thread retains {} events", ev.len());
    assert!(!ev.is_empty());
    cap.shutdown_blocking();
    wait_registry_gone(sid);

    eprintln!(
        "B5 answer: after write-back, turning LHC off leaves the user on the LHC-produced \
         native body. Pre-compaction native is not auto-restored. Older content survives in \
         the LHC event log (proven reopen) and host updates.jsonl (live track)."
    );

    set_use_deterministic_inference_for_test(false);
    set_compact_mode_for_test(None);
    drop(h);
    restore_env(prev, prev_root, prev_c, prev_e);
}

// ── B6 / B8 equivalence ─────────────────────────────────────────────────

#[test]
fn b6_equivalence_and_b8_encoding_and_tool_args() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let prev_c = std::env::var_os("GROK_LHC_COMPACT");
    let prev_e = std::env::var_os("GROK_LHC_COMPACT_EXPERIMENTAL");

    reset_equivalence_counters();
    let sid = "harness-b6-equiv";
    let native = multi_turn_native("sys", 12);
    let h = HarnessSession::open_replace(sid, native);
    let body = h.run_replace_writeback();

    // Real decide_substitution with argument-bearing tool call in native.
    let native_tools = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{\n  \"cmd\": \"ls\"\n}".into(),
        }]),
        ConversationItem::tool_result("c1", "ok"),
    ];
    // Build a minimal view from the write-back body via serve of empty→use fixture view.
    let (_n, fix) = adapter_simulated_writeback_body();
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            view_user("run", "u0"),
            view_assistant_tool("bash", r#"{"cmd":"ls"}"#, "a0"),
            view_tool_result("bash", "ok", "tr0"),
        ],
    };
    let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&view);
    let decision = decide_substitution(&native_tools, &view, &kinds, None);
    let (served, substituted) = apply_serve_decision(native_tools.clone(), decision);
    let obs = observe_serve_equivalence(sid, Some(0), true, substituted, &native_tools, &served);
    let snap = equivalence_snapshot();
    eprintln!(
        "B6/B8.1: compared={} fallen_back={} structural={} informational={} ratio={}:{} \
         obs_compared={} body_items={} fix_items={}",
        snap.turns_served_and_compared,
        snap.turns_fallen_back,
        snap.structural_divergences,
        snap.informational_divergences,
        snap.turns_fallen_back,
        snap.turns_served_and_compared,
        obs.compared,
        body.len(),
        fix.len()
    );
    assert!(
        snap.turns_served_and_compared > 0,
        "B8.1: must not read divergences without compared>0"
    );
    if snap.informational_divergences > 0 {
        eprintln!(
            "B6 FINDING: informational_divergences={} (report, do not fix)",
            snap.informational_divergences
        );
    }

    // B8.2 pre-registered encoding artifact
    reset_equivalence_counters();
    let native_raw = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("x"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{not json".into(),
        }]),
    ];
    let served_wrapped = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("x"),
        ConversationItem::assistant("[tool call · bash] {\"raw\":\"{not json\"}"),
    ];
    let rep = compare_serve_equivalence(&native_raw, &served_wrapped);
    let _ = observe_serve_equivalence(sid, Some(1), false, true, &native_raw, &served_wrapped);
    eprintln!(
        "B8.2 encoding artifact: structural={} informational={} (capture-encoding, not serving bug)",
        rep.structural_divergence, rep.informational_divergence
    );
    assert!(rep.informational_divergence || rep.structural_divergence);

    h.shutdown();
    restore_env(prev, prev_root, prev_c, prev_e);
}

// ── B7 performance ──────────────────────────────────────────────────────

#[test]
fn b7_perf_on_vs_off_and_compaction_wall() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let prev_c = std::env::var_os("GROK_LHC_COMPACT");
    let prev_e = std::env::var_os("GROK_LHC_COMPACT_EXPERIMENTAL");
    let root = TempDir::new().unwrap();

    unsafe {
        std::env::remove_var("GROK_LHC");
        std::env::set_var("GROK_LHC_ROOT", root.path());
    }
    let sid_off = "harness-b7-off";
    let (mock, _) = MockChatPersistence::new();
    let mut tee_off = tee_chat_persistence(sid_off, "/tmp", &[], Box::new(mock), None);
    let item = ConversationItem::user("perf");
    let t0 = Instant::now();
    for _ in 0..200 {
        tee_off.persist_message(&item);
    }
    let off_us = t0.elapsed().as_micros();
    drop(tee_off);

    unsafe { std::env::set_var("GROK_LHC", "1") };
    set_use_deterministic_inference_for_test(true);
    let sid_on = "harness-b7-on";
    let (mock, _) = MockChatPersistence::new();
    let mut tee_on = tee_chat_persistence(sid_on, "/tmp", &[], Box::new(mock), None);
    let cap = lookup_session(sid_on).expect("on capture");
    let t1 = Instant::now();
    for _ in 0..200 {
        tee_on.persist_message(&item);
    }
    tee_on.flush();
    let on_us = t1.elapsed().as_micros();
    let backlog = cap.list_events_blocking().map(|e| e.len()).unwrap_or(0);

    let native = multi_turn_native("sys", 12);
    let h = HarnessSession::open_replace("harness-b7-compact", native);
    let t2 = Instant::now();
    let _body = h.run_replace_writeback();
    let compact_ms = t2.elapsed().as_millis();
    let storage_bytes = std::fs::metadata(thread_file_path(h.root.path(), "harness-b7-compact"))
        .map(|m| m.len())
        .unwrap_or(0);

    eprintln!(
        "B7 perf: off_200={off_us}us on_200={on_us}us compact_wall={compact_ms}ms \
         storage_bytes={storage_bytes} on_events_after_200={backlog}"
    );

    drop(tee_on);
    cap.shutdown_blocking();
    wait_registry_gone(sid_on);
    h.shutdown();
    assert!(!capture_active(sid_on));
    assert!(!capture_active("harness-b7-compact"));
    restore_env(prev, prev_root, prev_c, prev_e);
}

// ── B8.4 carryables (verify) ────────────────────────────────────────────

#[test]
fn b8_carryable_grok_lhc_on_is_truthy() {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    unsafe { std::env::set_var("GROK_LHC", "on") };
    assert!(is_enabled(), "Y7: GROK_LHC=on must enable");
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
}

#[test]
fn b8_carryable_status_early_return_asymmetry_document() {
    // Confirmed by inspection in 3A: status_report gates on process-wide
    // any_capture_active; format_status_report off-branch uses session-local
    // capture_active. Harness documents claim scope — do not fix here.
    eprintln!(
        "B8 carryable: status early-return asymmetry remains — process-wide gate on \
         status_report vs session-local on format off-branch. Output correct; work may \
         exceed 'no I/O when off' when another session is capturing. Scope: claim is \
         'no I/O when process-wide off', not 'no I/O when this session off'."
    );
}
