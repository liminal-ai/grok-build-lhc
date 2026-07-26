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

use crate::capture::{CaptureHandle, spawn_capture};
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

/// Run the five hard write-back gates against `body` (sync / blocking RPCs).
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

    // (1) fixpoint
    {
        let sid = format!("{sid_prefix}-fixpoint");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let _ = wait_events(&handle, 1);
        handle.replace_history(body);
        handle.flush_blocking();
        let once = wait_events(&handle, 1);
        let once_keys = keys(&once);
        handle.replace_history(body);
        handle.flush_blocking();
        thread::sleep(Duration::from_millis(150));
        let again = handle.list_events_blocking().unwrap();
        assert_eq!(
            keys(&again),
            once_keys,
            "gate fixpoint ({label}): second replace re-keyed"
        );
        handle.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate fixpoint ({label}): PASS");
    }

    // (2) prune-shaped emits nothing
    {
        let sid = format!("{sid_prefix}-prune");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let _ = wait_events(&handle, 1);
        handle.replace_history(body);
        handle.flush_blocking();
        let after_wb = wait_events(&handle, 1);
        let before_keys = keys(&after_wb);
        let before_len = after_wb.len();
        let mut pruned: Vec<_> = body
            .iter()
            .filter(|i| matches!(i, ConversationItem::System(_)))
            .cloned()
            .collect();
        pruned.extend(body.iter().rev().take(2).cloned());
        assert!(pruned.len() < body.len() || body.len() <= 3);
        for _ in 0..3 {
            handle.replace_history(&pruned);
        }
        handle.flush_blocking();
        thread::sleep(Duration::from_millis(150));
        let after = handle.list_events_blocking().unwrap();
        assert_eq!(
            after.len(),
            before_len,
            "gate prune ({label}): emitted events"
        );
        assert_eq!(
            keys(&after),
            before_keys,
            "gate prune ({label}): key set changed"
        );
        handle.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate prune ({label}): PASS");
    }

    // (3) summary / novel content exactly once
    {
        let sid = format!("{sid_prefix}-summary");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let before = wait_events(&handle, 1);
        let before_keys = keys(&before);
        handle.replace_history(body);
        handle.flush_blocking();
        let after = wait_events(&handle, before.len() + 1);
        let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
        assert!(
            !new_keys.is_empty(),
            "gate summary ({label}): write-back recorded nothing"
        );
        if let Some(needle) = &band_needle {
            let hits = after
                .iter()
                .filter(|e| {
                    e.text_payload()
                        .is_some_and(|p| p.text.contains(needle.as_str()))
                })
                .count();
            assert_eq!(hits, 1, "gate summary ({label}): needle count {hits}");
        }
        handle.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate summary ({label}): PASS");
    }

    // (4) repeated unchanged nothing
    {
        let sid = format!("{sid_prefix}-repeat");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let _ = wait_events(&handle, 1);
        handle.replace_history(body);
        handle.flush_blocking();
        let once = wait_events(&handle, 1);
        let once_keys = keys(&once);
        let once_len = once.len();
        for _ in 0..4 {
            handle.replace_history(body);
        }
        handle.flush_blocking();
        thread::sleep(Duration::from_millis(150));
        let again = handle.list_events_blocking().unwrap();
        assert_eq!(again.len(), once_len, "gate repeat ({label}): length grew");
        assert_eq!(
            keys(&again),
            once_keys,
            "gate repeat ({label}): keys changed"
        );
        handle.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate repeat ({label}): PASS");
    }

    // (5) crash mid-replace no double
    {
        let sid = format!("{sid_prefix}-crash");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let seeded = wait_events(&handle, 1);
        let seeded_len = seeded.len();
        handle.arm_crash_mid_replace(1);
        handle.replace_history(body);
        wait_registry_gone(sid.as_str());
        thread::sleep(Duration::from_millis(200));
        let probe = spawn_capture(&sid, Some("/tmp"), &[], Some(root), None).unwrap();
        thread::sleep(Duration::from_millis(100));
        let partial = probe.list_events_blocking().unwrap();
        assert!(
            partial.len() > seeded_len,
            "gate crash ({label}): partial apply did not grow past seed"
        );
        probe.shutdown_blocking();
        wait_registry_gone(&sid);
        let handle2 = spawn_capture(&sid, Some("/tmp"), body, Some(root), None).unwrap();
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
        if let Some(needle) = &band_needle {
            let count = after_retry
                .iter()
                .filter(|e| {
                    e.text_payload()
                        .is_some_and(|p| p.text.contains(needle.as_str()))
                })
                .count();
            assert_eq!(
                count, 1,
                "gate crash ({label}): double-recorded after retry"
            );
        }
        handle2.shutdown_blocking();
        wait_registry_gone(&sid);
        eprintln!("gate crash ({label}): PASS");
    }
    eprintln!("=== all five hard gates PASS on {label} ===");
}

/// Async-safe five hard gates — safe to call from `#[tokio::test]`.
///
/// Uses awaitable capture RPCs only (never `blocking_send` / `blocking_recv`).
pub async fn run_five_gates_on_body_async(
    sid_prefix: &str,
    native: &[ConversationItem],
    body: &[ConversationItem],
    root: &Path,
    label: &str,
) {
    eprintln!("=== write-back hard gates (async) on {label} ===");
    let band_needle = band_needle(body);

    // (1) fixpoint
    {
        let sid = format!("{sid_prefix}-fixpoint");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let _ = wait_events_async(&handle, 1).await;
        handle.replace_history(body);
        handle.flush().await.expect("flush");
        let once = wait_events_async(&handle, 1).await;
        let once_keys = keys(&once);
        handle.replace_history(body);
        handle.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let again = handle.list_events().await.unwrap();
        assert_eq!(
            keys(&again),
            once_keys,
            "gate fixpoint ({label}): second replace re-keyed"
        );
        handle.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate fixpoint ({label}): PASS");
    }

    // (2) prune-shaped emits nothing
    {
        let sid = format!("{sid_prefix}-prune");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let _ = wait_events_async(&handle, 1).await;
        handle.replace_history(body);
        handle.flush().await.expect("flush");
        let after_wb = wait_events_async(&handle, 1).await;
        let before_keys = keys(&after_wb);
        let before_len = after_wb.len();
        let mut pruned: Vec<_> = body
            .iter()
            .filter(|i| matches!(i, ConversationItem::System(_)))
            .cloned()
            .collect();
        pruned.extend(body.iter().rev().take(2).cloned());
        assert!(pruned.len() < body.len() || body.len() <= 3);
        for _ in 0..3 {
            handle.replace_history(&pruned);
        }
        handle.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after = handle.list_events().await.unwrap();
        assert_eq!(
            after.len(),
            before_len,
            "gate prune ({label}): emitted events"
        );
        assert_eq!(
            keys(&after),
            before_keys,
            "gate prune ({label}): key set changed"
        );
        handle.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate prune ({label}): PASS");
    }

    // (3) summary / novel content exactly once
    {
        let sid = format!("{sid_prefix}-summary");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let before = wait_events_async(&handle, 1).await;
        let before_keys = keys(&before);
        handle.replace_history(body);
        handle.flush().await.expect("flush");
        let after = wait_events_async(&handle, before.len() + 1).await;
        let new_keys: BTreeSet<_> = keys(&after).difference(&before_keys).cloned().collect();
        assert!(
            !new_keys.is_empty(),
            "gate summary ({label}): write-back recorded nothing"
        );
        if let Some(needle) = &band_needle {
            let hits = after
                .iter()
                .filter(|e| {
                    e.text_payload()
                        .is_some_and(|p| p.text.contains(needle.as_str()))
                })
                .count();
            assert_eq!(hits, 1, "gate summary ({label}): needle count {hits}");
        }
        handle.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate summary ({label}): PASS");
    }

    // (4) repeated unchanged nothing
    {
        let sid = format!("{sid_prefix}-repeat");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let _ = wait_events_async(&handle, 1).await;
        handle.replace_history(body);
        handle.flush().await.expect("flush");
        let once = wait_events_async(&handle, 1).await;
        let once_keys = keys(&once);
        let once_len = once.len();
        for _ in 0..4 {
            handle.replace_history(body);
        }
        handle.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let again = handle.list_events().await.unwrap();
        assert_eq!(again.len(), once_len, "gate repeat ({label}): length grew");
        assert_eq!(
            keys(&again),
            once_keys,
            "gate repeat ({label}): keys changed"
        );
        handle.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate repeat ({label}): PASS");
    }

    // (5) crash mid-replace no double
    {
        let sid = format!("{sid_prefix}-crash");
        let handle = spawn_capture(&sid, Some("/tmp"), native, Some(root), None).unwrap();
        let seeded = wait_events_async(&handle, 1).await;
        let seeded_len = seeded.len();
        handle.arm_crash_mid_replace(1);
        handle.replace_history(body);
        wait_registry_gone_async(sid.as_str()).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let probe = spawn_capture(&sid, Some("/tmp"), &[], Some(root), None).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let partial = probe.list_events().await.unwrap();
        assert!(
            partial.len() > seeded_len,
            "gate crash ({label}): partial apply did not grow past seed"
        );
        probe.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        let handle2 = spawn_capture(&sid, Some("/tmp"), body, Some(root), None).unwrap();
        handle2.flush().await.expect("flush");
        let mut after_retry = wait_events_async(&handle2, seeded_len).await;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let cur = handle2.list_events().await.unwrap();
            if cur.len() == after_retry.len() && cur.len() > seeded_len {
                after_retry = cur;
                break;
            }
            after_retry = cur;
        }
        if let Some(needle) = &band_needle {
            let count = after_retry
                .iter()
                .filter(|e| {
                    e.text_payload()
                        .is_some_and(|p| p.text.contains(needle.as_str()))
                })
                .count();
            assert_eq!(
                count, 1,
                "gate crash ({label}): double-recorded after retry"
            );
        }
        handle2.shutdown().await.expect("shutdown");
        wait_registry_gone_async(&sid).await;
        eprintln!("gate crash ({label}): PASS");
    }
    eprintln!("=== all five hard gates PASS on {label} ===");
}
