//! Five hard write-back gates + helpers (test-util).
//!
//! Shared by `harness_chunk3b` (deterministic body) and credentialed G2
//! (real-inference body) so both subjects run the same instrument.
//!
//! Sync [`run_five_gates_on_body`] uses blocking RPCs — **must not** be called
//! from inside a Tokio runtime. Async tests use
//! [`run_five_gates_on_body_async`].

use std::collections::BTreeSet;
use std::path::Path;
use std::thread;
use std::time::Duration;

use lhc::intake_stream::EventRecord;
use xai_grok_sampling_types::ConversationItem;

use crate::capture::{CaptureHandle, spawn_capture, spawn_capture_resumed};
use crate::generated_prefix::GeneratedPrefix;
use crate::tee::capture_active;

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
    handle
        .list_events()
        .await
        .expect("list_events after wait (async)")
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

async fn wait_registry_gone_async(session_id: &str) {
    for _ in 0..120 {
        if !capture_active(session_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("registry entry still present for {session_id}");
}

fn keys(events: &[EventRecord]) -> BTreeSet<String> {
    events
        .iter()
        .map(|e| e.idempotency_key().to_string())
        .collect()
}

fn band_needle(body: &[ConversationItem]) -> Option<String> {
    body.iter().find_map(|i| {
        let t = i.text_content();
        if t.contains("[context") {
            Some(t)
        } else {
            None
        }
    })
}

fn needle_count(events: &[EventRecord], needle: Option<&String>) -> usize {
    let Some(needle) = needle else {
        return 0;
    };
    events
        .iter()
        .filter(|e| {
            e.prompt_or_note_text()
                .is_some_and(|t| t.contains(needle.as_str()))
        })
        .count()
}

/// The canonical tip a freshly bootstrapped gate session was generated from:
/// its highest recorded event order (what the worker latches as `generation`).
fn source_tip(events: &[EventRecord]) -> u64 {
    events
        .iter()
        .map(|e| e.event_order())
        .max()
        .unwrap_or(0)
        .max(0) as u64
}

/// Genuine work appended after the write-back: a prompt, the same prompt
/// again (repeated real prompts are ordinary), and a closing reply.
fn genuine_suffix() -> Vec<ConversationItem> {
    vec![
        ConversationItem::user("after write-back: continue"),
        ConversationItem::user("after write-back: continue"),
        ConversationItem::assistant("continued"),
    ]
}

/// A production-shaped in-place prune of the body: the first ToolResult is
/// hard-cleared under its own call id. `None` when the body carries no tool
/// result (the prune gate then rewrites the System head instead).
fn pruned_in_place(body: &[ConversationItem]) -> Option<Vec<ConversationItem>> {
    let idx = body
        .iter()
        .position(|i| matches!(i, ConversationItem::ToolResult(_)))?;
    let mut pruned = body.to_vec();
    if let ConversationItem::ToolResult(tr) = &body[idx] {
        pruned[idx] = ConversationItem::tool_result(&tr.tool_call_id, "[cleared]");
    }
    Some(pruned)
}

/// Run the five hard write-back gates against `body` (sync / blocking RPCs).
///
/// Production route (slice 1A): the body is *installed* (`writeback_installed`
/// with the source tip), never submitted as source; genuine whole-history
/// replaces after it re-map through the installed prefix. Each gate opens its
/// own session on `native` and derives the tip from that session's record.
///
/// **Do not call from an async Tokio test** — use
/// [`run_five_gates_on_body_async`]. `label` is printed so readers can tell
/// which body (deterministic vs credentialed) produced the results.
pub fn run_five_gates_on_body(
    sid_prefix: &str,
    native: &[ConversationItem],
    body: &[ConversationItem],
    root: &Path,
    label: &str,
) {
    eprintln!("=== write-back hard gates on {label} ===");
    let band_needle = band_needle(body);
    let needle = band_needle.as_ref();

    // (1) install fixpoint: installing the body, again, and replacing with it
    // records nothing and re-keys nothing.
    {
        let sid = format!("{sid_prefix}-fixpoint");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let seeded = wait_events(&handle, 1);
        let seeded_keys = keys(&seeded);
        let tip = source_tip(&seeded);
        handle.writeback_installed(body, tip);
        handle.writeback_installed(body, tip);
        handle.replace_history(body);
        handle.flush_blocking();
        thread::sleep(Duration::from_millis(150));
        let again = handle.list_events_blocking().unwrap();
        assert_eq!(
            keys(&again),
            seeded_keys,
            "gate fixpoint ({label}): install/replace of the body changed keys"
        );
        handle.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate fixpoint ({label}): PASS");
    }

    // (2) in-place prune / head rewrite after install records only the
    // changed head (never the body).
    {
        let sid = format!("{sid_prefix}-prune");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let seeded = wait_events(&handle, 1);
        let before_keys = keys(&seeded);
        let tip = source_tip(&seeded);
        handle.writeback_installed(body, tip);
        let (rewritten, expected_new) = match pruned_in_place(body) {
            Some(pruned) => (pruned, 0usize),
            None => {
                let mut head = body.to_vec();
                head[0] = ConversationItem::system("sys + memory reminder (gate)");
                (head, 1usize)
            }
        };
        for _ in 0..3 {
            handle.replace_history(&rewritten);
        }
        handle.flush_blocking();
        thread::sleep(Duration::from_millis(150));
        let after = handle.list_events_blocking().unwrap();
        let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
        assert_eq!(
            new_keys.len(),
            expected_new,
            "gate prune ({label}): in-place rewrite recorded {} events",
            new_keys.len()
        );
        assert_eq!(
            needle_count(&after, needle),
            0,
            "gate prune ({label}): band entered the record"
        );
        handle.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate prune ({label}): PASS");
    }

    // (3) generated band never enters the canonical record; genuine suffix
    // after it does, exactly once, through a whole-history replace.
    {
        let sid = format!("{sid_prefix}-summary");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let before = wait_events(&handle, 1);
        let before_keys = keys(&before);
        let tip = source_tip(&before);
        handle.writeback_installed(body, tip);
        let mut with_suffix = body.to_vec();
        with_suffix.extend(genuine_suffix());
        handle.replace_history(&with_suffix);
        handle.flush_blocking();
        let after = wait_events(&handle, before.len() + 1);
        let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
        assert_eq!(
            new_keys.len(),
            genuine_suffix().len() + 1,
            "gate summary ({label}): expected the suffix (+ its turn_end) only, got {new_keys:?}"
        );
        assert_eq!(
            needle_count(&after, needle),
            0,
            "gate summary ({label}): generated band recorded as source"
        );
        handle.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate summary ({label}): PASS");
    }

    // (4) repeated unchanged: repeated installs and body replaces record nothing.
    {
        let sid = format!("{sid_prefix}-repeat");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let once = wait_events(&handle, 1);
        let once_keys = keys(&once);
        let tip = source_tip(&once);
        for _ in 0..4 {
            handle.writeback_installed(body, tip);
            handle.replace_history(body);
        }
        handle.flush_blocking();
        thread::sleep(Duration::from_millis(150));
        let again = handle.list_events_blocking().unwrap();
        assert_eq!(
            again.len(),
            once.len(),
            "gate repeat ({label}): length grew"
        );
        assert_eq!(
            keys(&again),
            once_keys,
            "gate repeat ({label}): keys changed"
        );
        handle.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate repeat ({label}): PASS");
    }

    // (5) crash mid-replace, resume with the marked body: the uncaptured
    // suffix is recovered, the captured suffix dedups, nothing doubles, and
    // the band still never enters the record.
    {
        let sid = format!("{sid_prefix}-crash");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let seeded = wait_events(&handle, 1);
        let seeded_len = seeded.len();
        let tip = source_tip(&seeded);
        handle.writeback_installed(body, tip);
        let mut with_suffix = body.to_vec();
        with_suffix.extend(genuine_suffix());
        handle.arm_crash_mid_replace(1);
        handle.replace_history(&with_suffix);
        wait_registry_gone(sid.as_str());
        thread::sleep(Duration::from_millis(200));
        let probe = spawn_capture(&sid, Some("/tmp"), &[], Some(root), None).unwrap();
        thread::sleep(Duration::from_millis(100));
        let partial = probe.list_events_blocking().unwrap();
        assert!(
            partial.len() > seeded_len,
            "gate crash ({label}): partial apply did not grow past seed"
        );
        assert_eq!(
            needle_count(&partial, needle),
            0,
            "gate crash ({label}): band recorded before the crash"
        );
        probe.shutdown_blocking();
        wait_registry_gone(&sid);
        // Resume: native holds body + suffix; the checkpoint marks the body.
        let generated = GeneratedPrefix {
            items: body.to_vec(),
            source_tip: tip,
        };
        let handle2 = spawn_capture_resumed(
            &sid,
            Some("/tmp"),
            &with_suffix,
            Some(generated.clone()),
            Some(root),
            None,
        )
        .unwrap();
        handle2.flush_blocking();
        let after_retry = wait_events(&handle2, seeded_len + genuine_suffix().len() + 1);
        let keys_once = keys(&after_retry);
        assert_eq!(
            after_retry.len(),
            seeded_len + genuine_suffix().len() + 1,
            "gate crash ({label}): suffix (+ turn_end) not recovered exactly once"
        );
        assert_eq!(
            needle_count(&after_retry, needle),
            0,
            "gate crash ({label}): band recorded on resume"
        );
        handle2.replace_history(&with_suffix);
        handle2.flush_blocking();
        thread::sleep(Duration::from_millis(150));
        let again = handle2.list_events_blocking().unwrap();
        assert_eq!(
            keys(&again),
            keys_once,
            "gate crash ({label}): re-keyed after resume"
        );
        handle2.shutdown_blocking();
        wait_registry_gone(&sid);
        // Second resume records nothing.
        let handle3 = spawn_capture_resumed(
            &sid,
            Some("/tmp"),
            &with_suffix,
            Some(generated),
            Some(root),
            None,
        )
        .unwrap();
        handle3.flush_blocking();
        thread::sleep(Duration::from_millis(150));
        let third = handle3.list_events_blocking().unwrap();
        assert_eq!(
            keys(&third),
            keys_once,
            "gate crash ({label}): second resume recorded"
        );
        handle3.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate crash ({label}): PASS");
    }
    eprintln!("=== all five hard gates PASS on {label} ===");
}

/// Async-safe five hard gates — safe to call from `#[tokio::test]`.
///
/// Same gates as [`run_five_gates_on_body`] over awaitable capture RPCs only
/// (never `blocking_send` / `blocking_recv`).
pub async fn run_five_gates_on_body_async(
    sid_prefix: &str,
    native: &[ConversationItem],
    body: &[ConversationItem],
    root: &Path,
    label: &str,
) {
    eprintln!("=== write-back hard gates (async) on {label} ===");
    let band_needle = band_needle(body);
    let needle = band_needle.as_ref();

    // (1) install fixpoint
    {
        let sid = format!("{sid_prefix}-fixpoint");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let seeded = wait_events_async(&handle, 1).await;
        let seeded_keys = keys(&seeded);
        let tip = source_tip(&seeded);
        handle.writeback_installed(body, tip);
        handle.writeback_installed(body, tip);
        handle.replace_history(body);
        handle.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let again = handle.list_events().await.unwrap();
        assert_eq!(
            keys(&again),
            seeded_keys,
            "gate fixpoint ({label}): install/replace of the body changed keys"
        );
        handle.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate fixpoint ({label}): PASS");
    }

    // (2) in-place prune / head rewrite
    {
        let sid = format!("{sid_prefix}-prune");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let seeded = wait_events_async(&handle, 1).await;
        let before_keys = keys(&seeded);
        let tip = source_tip(&seeded);
        handle.writeback_installed(body, tip);
        let (rewritten, expected_new) = match pruned_in_place(body) {
            Some(pruned) => (pruned, 0usize),
            None => {
                let mut head = body.to_vec();
                head[0] = ConversationItem::system("sys + memory reminder (gate)");
                (head, 1usize)
            }
        };
        for _ in 0..3 {
            handle.replace_history(&rewritten);
        }
        handle.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after = handle.list_events().await.unwrap();
        let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
        assert_eq!(
            new_keys.len(),
            expected_new,
            "gate prune ({label}): in-place rewrite recorded {} events",
            new_keys.len()
        );
        assert_eq!(
            needle_count(&after, needle),
            0,
            "gate prune ({label}): band recorded"
        );
        handle.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate prune ({label}): PASS");
    }

    // (3) band never canonical; suffix exactly once
    {
        let sid = format!("{sid_prefix}-summary");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let before = wait_events_async(&handle, 1).await;
        let before_keys = keys(&before);
        let tip = source_tip(&before);
        handle.writeback_installed(body, tip);
        let mut with_suffix = body.to_vec();
        with_suffix.extend(genuine_suffix());
        handle.replace_history(&with_suffix);
        handle.flush().await.expect("flush");
        let after = wait_events_async(&handle, before.len() + 1).await;
        let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
        assert_eq!(
            new_keys.len(),
            genuine_suffix().len() + 1,
            "gate summary ({label}): expected the suffix (+ its turn_end) only, got {new_keys:?}"
        );
        assert_eq!(
            needle_count(&after, needle),
            0,
            "gate summary ({label}): generated band recorded as source"
        );
        handle.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate summary ({label}): PASS");
    }

    // (4) repeated unchanged nothing
    {
        let sid = format!("{sid_prefix}-repeat");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let once = wait_events_async(&handle, 1).await;
        let once_keys = keys(&once);
        let tip = source_tip(&once);
        for _ in 0..4 {
            handle.writeback_installed(body, tip);
            handle.replace_history(body);
        }
        handle.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let again = handle.list_events().await.unwrap();
        assert_eq!(
            again.len(),
            once.len(),
            "gate repeat ({label}): length grew"
        );
        assert_eq!(
            keys(&again),
            once_keys,
            "gate repeat ({label}): keys changed"
        );
        handle.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate repeat ({label}): PASS");
    }

    // (5) crash mid-replace, resume with the marked body
    {
        let sid = format!("{sid_prefix}-crash");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let seeded = wait_events_async(&handle, 1).await;
        let seeded_len = seeded.len();
        let tip = source_tip(&seeded);
        handle.writeback_installed(body, tip);
        let mut with_suffix = body.to_vec();
        with_suffix.extend(genuine_suffix());
        handle.arm_crash_mid_replace(1);
        handle.replace_history(&with_suffix);
        wait_registry_gone_async(sid.as_str()).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let probe = spawn_capture(&sid, Some("/tmp"), &[], Some(root), None).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let partial = probe.list_events().await.unwrap();
        assert!(
            partial.len() > seeded_len,
            "gate crash ({label}): partial apply did not grow past seed"
        );
        assert_eq!(
            needle_count(&partial, needle),
            0,
            "gate crash ({label}): band before crash"
        );
        probe.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        let generated = GeneratedPrefix {
            items: body.to_vec(),
            source_tip: tip,
        };
        let handle2 = spawn_capture_resumed(
            &sid,
            Some("/tmp"),
            &with_suffix,
            Some(generated.clone()),
            Some(root),
            None,
        )
        .unwrap();
        handle2.flush().await.expect("flush");
        let after_retry =
            wait_events_async(&handle2, seeded_len + genuine_suffix().len() + 1).await;
        let keys_once = keys(&after_retry);
        assert_eq!(
            after_retry.len(),
            seeded_len + genuine_suffix().len() + 1,
            "gate crash ({label}): suffix (+ turn_end) not recovered exactly once"
        );
        assert_eq!(
            needle_count(&after_retry, needle),
            0,
            "gate crash ({label}): band on resume"
        );
        handle2.replace_history(&with_suffix);
        handle2.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let again = handle2.list_events().await.unwrap();
        assert_eq!(
            keys(&again),
            keys_once,
            "gate crash ({label}): re-keyed after resume"
        );
        handle2.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        let handle3 = spawn_capture_resumed(
            &sid,
            Some("/tmp"),
            &with_suffix,
            Some(generated),
            Some(root),
            None,
        )
        .unwrap();
        handle3.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let third = handle3.list_events().await.unwrap();
        assert_eq!(
            keys(&third),
            keys_once,
            "gate crash ({label}): second resume recorded"
        );
        handle3.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate crash ({label}): PASS");
    }
    eprintln!("=== all five hard gates PASS on {label} ===");
}
