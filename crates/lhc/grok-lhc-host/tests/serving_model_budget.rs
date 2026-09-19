//! E2: serving-model weights on budget reads, raw storage unchanged.
//!
//! Pin `aa9caa16` is dead weight unless the host calls SDK `set_model` with the
//! live chat model on create, resume, and switch — never the derivation model.

use std::time::Duration;

use grok_lhc_host::{
    CaptureHandle, capture_model_or_thinking_change, env_lock, estimate_tokens, spawn_capture,
    spawn_capture_with_serving_model, thread_file_path,
};
use tempfile::TempDir;
use xai_grok_sampling_types::ConversationItem;

fn weigh(raw: i64) -> i64 {
    (raw * 21 + 19) / 20
}

fn with_root<T>(root: &std::path::Path, f: impl FnOnce() -> T) -> T {
    let _g = env_lock();
    let prev = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root);
    }
    let out = f();
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
        None => unsafe { std::env::remove_var("GROK_LHC") },
    }
    match prev_root {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
    }
    out
}

fn wait_open(handle: &grok_lhc_host::CaptureHandle) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(handle.wait_until_open(Duration::from_secs(5)))
        .expect("archive open");
}

fn tail_tokens(handle: &grok_lhc_host::CaptureHandle) -> i64 {
    handle.flush_blocking();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(handle.get_view_status())
        .expect("view status")
        .tail_tokens
}

fn wait_events(handle: &CaptureHandle, min: usize) {
    for _ in 0..200 {
        if let Ok(ev) = handle.list_events_blocking()
            && ev.len() >= min
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for {min} events: {:?}",
        handle.list_events_blocking().map(|e| e.len())
    );
}

fn stored_message_tokens(root: &std::path::Path, sid: &str) -> Vec<i64> {
    let db = rusqlite::Connection::open(thread_file_path(root, sid)).unwrap();
    let mut stmt = db
        .prepare("SELECT token_estimate FROM message ORDER BY message_id")
        .unwrap();
    stmt.query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn grok_serving_model_weights_budget_reads_and_leaves_raw_storage() {
    let root = TempDir::new().unwrap();
    let sid = "e2-serving-create";
    let text = "E2-WEIGHT unique payload ".repeat(40);
    let raw = estimate_tokens(&text);
    assert!(raw > 20, "fixture must distinguish 1.05x from identity");
    with_root(root.path(), || {
        let handle = spawn_capture_with_serving_model(
            sid,
            Some("/tmp"),
            &[],
            None,
            Some(root.path()),
            None,
            Some("grok-4.6"),
        )
        .unwrap();
        wait_open(&handle);
        handle.persist(&ConversationItem::user(&text));
        handle.flush_blocking();
        wait_events(&handle, 1);
        let stored = stored_message_tokens(root.path(), sid);
        let raw_sum: i64 = stored.iter().sum();
        assert!(
            stored.iter().any(|&t| t == raw),
            "raw message token_count must stay o200k: {stored:?} raw={raw}"
        );
        assert_eq!(
            tail_tokens(&handle),
            weigh(raw_sum),
            "create must set grok serving family: raw_sum={raw_sum} stored={stored:?}"
        );

        capture_model_or_thinking_change(sid, "grok-4.6", "gpt-5.5", None, None);
        handle.flush_blocking();
        wait_events(&handle, 1);
        let stored_gpt = stored_message_tokens(root.path(), sid);
        let raw_gpt: i64 = stored_gpt.iter().sum();
        assert_eq!(
            tail_tokens(&handle),
            raw_gpt,
            "switch to non-grok serving family must drop the 1.05 weight"
        );
        assert_eq!(
            stored_gpt.iter().find(|&&t| t == raw),
            Some(&raw),
            "original user token_estimate must stay raw through the switch"
        );

        capture_model_or_thinking_change(sid, "gpt-5.5", "grok-4.5", None, None);
        handle.flush_blocking();
        wait_events(&handle, 1);
        let stored_grok = stored_message_tokens(root.path(), sid);
        let raw_grok: i64 = stored_grok.iter().sum();
        assert_eq!(
            tail_tokens(&handle),
            weigh(raw_grok),
            "switch back to grok serving family must weigh again"
        );
        assert_eq!(
            stored_grok.iter().find(|&&t| t == raw),
            Some(&raw),
            "original user token_estimate must stay raw after switching back"
        );
        handle.shutdown_blocking();
    });
}

#[test]
fn resume_reapplies_serving_model_and_unspecified_stays_o200k() {
    let root = TempDir::new().unwrap();
    let sid = "e2-serving-resume";
    let text = "E2-RESUME unique payload ".repeat(40);
    let raw = estimate_tokens(&text);
    assert!(raw > 20);
    with_root(root.path(), || {
        let handle = spawn_capture_with_serving_model(
            sid,
            Some("/tmp"),
            &[ConversationItem::user(&text)],
            None,
            Some(root.path()),
            None,
            Some("grok-4.6"),
        )
        .unwrap();
        wait_open(&handle);
        handle.flush_blocking();
        wait_events(&handle, 1);
        let stored = stored_message_tokens(root.path(), sid);
        let raw_sum: i64 = stored.iter().sum();
        assert_eq!(tail_tokens(&handle), weigh(raw_sum));
        handle.shutdown_blocking();

        let resumed = spawn_capture_with_serving_model(
            sid,
            Some("/tmp"),
            &[ConversationItem::user(&text)],
            None,
            Some(root.path()),
            None,
            Some("grok-4.6"),
        )
        .unwrap();
        wait_open(&resumed);
        resumed.flush_blocking();
        assert_eq!(
            tail_tokens(&resumed),
            weigh(raw_sum),
            "resume must set_model again; a new Lhc defaults to o200k"
        );
        assert_eq!(stored_message_tokens(root.path(), sid), stored);
        resumed.shutdown_blocking();

        let unspecified = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
        wait_open(&unspecified);
        unspecified.flush_blocking();
        assert_eq!(
            tail_tokens(&unspecified),
            raw_sum,
            "omitting serving model must not inherit the derivation grok-4.5 family"
        );
        unspecified.shutdown_blocking();
    });
}
