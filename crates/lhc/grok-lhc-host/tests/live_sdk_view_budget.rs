//! Open a live fold thread and print the SDK ViewStatus / CompactReceipt.
//!
//! Not posthoc 1.05× storage. Requires:
//! `D_LIVE_FOLD_ROOT` = LHC root containing `threads/grok-<sid>.sqlite`
//! `D_LIVE_FOLD_SID` = ACP session id
//! `D_LIVE_FOLD_MODEL` = serving model (default grok-4.5)

use std::path::{Path, PathBuf};
use std::time::Duration;

use grok_lhc_host::{env_lock, spawn_capture_with_serving_model, thread_file_path};

fn with_root<T>(root: &Path, f: impl FnOnce() -> T) -> T {
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

#[test]
fn live_fold_sdk_view_and_compact_receipt() {
    let src_root = match std::env::var("D_LIVE_FOLD_ROOT") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            eprintln!("SKIP live_sdk_view_budget: D_LIVE_FOLD_ROOT unset");
            return;
        }
    };
    let sid = match std::env::var("D_LIVE_FOLD_SID") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("SKIP live_sdk_view_budget: D_LIVE_FOLD_SID unset");
            return;
        }
    };
    let model = std::env::var("D_LIVE_FOLD_MODEL").unwrap_or_else(|_| "grok-4.5".into());
    with_root(&src_root, || {
        let handle = spawn_capture_with_serving_model(
            &sid,
            Some("/tmp"),
            &[],
            None,
            Some(src_root.as_path()),
            None,
            Some(model.as_str()),
        )
        .expect("spawn live capture");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(handle.wait_until_open(Duration::from_secs(10)))
            .expect("archive open");
        handle.flush_blocking();
        let vs = rt
            .block_on(handle.get_view_status())
            .expect("SDK ViewStatus");
        println!(
            "SDK_VIEW_STATUS serving_model={model} tail_tokens={} threshold={} compact_recommended={} pending={} failed={} blocked={} zone_tokens={} max_tokens={} view={:?}",
            vs.tail_tokens,
            vs.threshold,
            vs.compact_recommended,
            vs.derivation.pending,
            vs.derivation.failed,
            vs.derivation.blocked,
            vs.visibility.zone_tokens,
            vs.visibility.max_tokens,
            vs.view
        );
        let receipt = rt
            .block_on(handle.compact_thread())
            .expect("SDK CompactReceipt");
        println!(
            "SDK_COMPACT_RECEIPT view_id={} profile={:?} compact_point={} covered_from={} tail_tokens={} total_tokens={} lower_bound={} bands_brief={} bands_detailed={} bands_smooth={}",
            receipt.view_id,
            receipt.profile,
            receipt.compact_point,
            receipt.covered_from,
            receipt.tail_tokens,
            receipt.total_tokens,
            receipt.config.lower_bound,
            receipt.bands.brief.tokens,
            receipt.bands.detailed.tokens,
            receipt.bands.smooth.tokens,
        );
        let db = rusqlite::Connection::open(thread_file_path(&src_root, &sid)).unwrap();
        let raw_sum: i64 = db
            .query_row(
                "SELECT COALESCE(SUM(token_estimate),0) FROM message",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))
            .unwrap();
        let estimator: String = db
            .query_row("SELECT token_estimator FROM thread_metadata", [], |row| {
                row.get(0)
            })
            .unwrap();
        println!("RAW_STORAGE messages={n} token_estimate_sum={raw_sum} estimator={estimator}");
        println!(
            "VIEW_IDENTITY sid={sid} sqlite={}",
            thread_file_path(&src_root, &sid).display()
        );
        assert!(
            vs.tail_tokens > 0,
            "SDK ViewStatus.tail_tokens must be a live budget, got {}",
            vs.tail_tokens
        );
        assert!(
            receipt.total_tokens > 0,
            "SDK CompactReceipt.total_tokens must be a live assembled budget, got {}",
            receipt.total_tokens
        );
        handle.shutdown_blocking();
    });
}
