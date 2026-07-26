//! Chunk 3B / unit 19 — G2 against **real** derivation inference.
//!
//! Uses [`ShellLhcInferenceSampler`] (native sampler, `grok-4.5` @ low
//! thinking). Deterministic callbacks stay in `grok-lhc-host`'s
//! `harness_chunk3b` for mechanism-only coverage.
//!
//! **Ignored by default** (`#[ignore]`): needs a live bearer and several
//! minutes of network. Run via LIVE_RUNBOOK L1:
//! `cargo test -p xai-grok-shell --lib l3_g2_real_inference -- --ignored --nocapture`
//!
//! Production path: do **not** pre-drain — [`replace_compact_for_writeback`]
//! is selection + fallback ladder only (Background mode drains on the
//! scheduler; compact never waits on derivation). Degraded rungs are a
//! designed receipt outcome, not a hard fail.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use grok_lhc_host::{
    LhcInferenceOp, LhcInferenceRequest, LhcInferenceSampler, build_writeback_conversation,
    compare_serve_equivalence, equivalence_snapshot, estimate_tokens, observe_serve_equivalence,
    project_conversation_canonical, replace_compact_for_writeback, reset_equivalence_counters,
    run_five_gates_on_body_async, spawn_capture,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use xai_grok_sampling_types::ConversationItem;

use crate::agent::config::{CLI_CHAT_PROXY_BASE_URL_DEFAULT, EndpointsConfig};
use crate::auth::{AuthManager, GrokComConfig};
use crate::session::lhc_inference::ShellLhcInferenceSampler;
use xai_grok_sampler::SamplerConfig;

/// Production built-in `continuation` profile (params: None).
const PROD_LOWER_BOUND: f64 = 120_000.0;
const PROD_FULL_PERCENT: f64 = 30.0;
const PROD_FULL_BUDGET: f64 = PROD_LOWER_BOUND * PROD_FULL_PERCENT / 100.0; // 36_000

/// Seed sizing: enough closed-turn tokens that newest-first fill crosses
/// `PROD_FULL_BUDGET`. Fewer, larger turns keep PromptSmoothing round-trips
/// manageable under Background drain.
const SEED_TURNS: usize = 6;
const SEED_WORDS_PER_MSG: usize = 5_000;

/// Compact is selection-only after the architecture repair — fractions of a
/// second when ready≈total. Hundreds of seconds ⇒ drain wrongly back on compact.
const COMPACT_FAST_CEILING: Duration = Duration::from_secs(30);

fn grok_home() -> PathBuf {
    dirs::home_dir().expect("HOME").join(".grok")
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
            let text = i.text_content();
            let short = if text.len() > 96 {
                format!("{}…", &text[..96])
            } else {
                text
            };
            format!("{kind}:{short}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn adapter_fixture_fingerprint() -> String {
    [
        "system:sys",
        "user_meta:[context · brief]",
        "user:please investigate area 9",
        "assistant_tools:",
        "tool_result:file_a",
        "user_meta:[runtime note]",
        "user_meta:[model change]",
        "user:live-2",
        "assistant:a2",
    ]
    .join("\n")
}

fn multi_turn_native(turns: usize, words_per_msg: usize) -> Vec<ConversationItem> {
    let blob = "word ".repeat(words_per_msg);
    let mut items = vec![ConversationItem::system("sys")];
    for t in 0..turns {
        let mut u = ConversationItem::user(format!("turn {t} {blob}"));
        u.set_prompt_index(t);
        items.push(u);
        if t == 1 {
            items.push(ConversationItem::assistant_tool_calls(vec![
                xai_grok_sampling_types::ToolCall {
                    id: format!("c{t}").into(),
                    name: "bash".into(),
                    arguments: "{\"cmd\":\"ls\"}".into(),
                },
            ]));
            items.push(ConversationItem::tool_result(
                format!("c{t}"),
                "file_a\nfile_b",
            ));
        }
        items.push(ConversationItem::assistant(format!("answer {t} {blob}")));
    }
    items
}

fn estimated_seed_tokens(items: &[ConversationItem]) -> i64 {
    items
        .iter()
        .map(|i| estimate_tokens(&i.text_content()))
        .sum()
}

fn band_count(body: &[ConversationItem]) -> usize {
    body.iter()
        .filter(|i| i.text_content().contains("[context"))
        .count()
}

/// Report degraded body markers + receipt rungs (designed fallback ladder).
fn report_degraded(body: &[ConversationItem], receipt: &lhc::shared_tech::view::CompactReceipt) {
    let body_markers: Vec<_> = body
        .iter()
        .map(|i| i.text_content())
        .filter(|t| t.contains("[degraded:"))
        .collect();
    eprintln!(
        "L3 G2 DEGRADED (reportable, not failing): body_markers={} \
         receipt.degraded={} receipt.gaps={}",
        body_markers.len(),
        receipt.degraded.len(),
        receipt.gaps.len()
    );
    for t in &body_markers {
        let line = t.lines().next().unwrap_or(t);
        eprintln!("  body: {line}");
    }
    for d in &receipt.degraded {
        eprintln!(
            "  receipt.degraded band={:?} subject={} used_derivation={}",
            d.band, d.subject_id, d.used_derivation
        );
    }
    for g in &receipt.gaps {
        eprintln!(
            "  receipt.gap band={:?} subject={} reason={}",
            g.band, g.subject_id, g.reason
        );
    }
    if !body_markers.is_empty() {
        assert!(
            !receipt.degraded.is_empty() || !receipt.gaps.is_empty(),
            "body has [degraded:] markers but receipt recorded neither \
             degraded nor gaps — fallback ladder must account in the receipt"
        );
    }
}

/// Probe credentials + native sampler before the expensive G2 path.
///
/// PromptSmoothing probe = production lane. ToolResultSummary probe =
/// **direct-call capability only** — unreachable from production Background
/// drain under DERIV-12 (`FORCE_TOOL_RESULT_SUMMARY_FALLBACK`).
async fn probe_real_sampler(sampler: &ShellLhcInferenceSampler) -> Result<String, String> {
    assert_eq!(
        sampler.model_slug(),
        "grok-4.5",
        "real-inference G2 must use grok-4.5 default"
    );
    let probe_req = LhcInferenceRequest::SmoothPrompt {
        text: "Rewrite briefly: the cat sat on the mat.".into(),
        max_output_tokens: 64,
    };
    let built = sampler.build_derivation_request(&probe_req, &sampler.model_slug());
    assert_eq!(
        built.reasoning_effort,
        Some(xai_grok_sampling_types::ReasoningEffort::Low),
        "derivation ConversationRequest must carry ReasoningEffort::Low"
    );

    // Production lane.
    match sampler.sample(probe_req, CancellationToken::new()).await {
        Ok(sample) => {
            eprintln!(
                "L3 probe OK [PromptSmoothing / production lane]: model={} \
                 label={} text_len={}",
                sample.model,
                sample.prompt_label,
                sample.text.len()
            );
            assert_eq!(
                sample.model, "grok-4.5",
                "PromptSmoothing must use grok-4.5"
            );
        }
        Err(err) => {
            return Err(format!(
                "BLOCKED: real derivation inference unavailable on PromptSmoothing \
                 ({:?}: {}). Tried ShellLhcInferenceSampler against ~/.grok auth + {}. \
                 Do not substitute deterministic callbacks for this cert.",
                err.kind, err.detail, CLI_CHAT_PROXY_BASE_URL_DEFAULT
            ));
        }
    }

    // Direct-call capability probe — NOT a production Background drain lane
    // (DERIV-12: ToolResultSummary stays on truncate-fallback).
    let tool = LhcInferenceRequest::SummarizeToolResult {
        tool_name: "bash".into(),
        content: "file_a\nfile_b".into(),
        outcome: None,
        target_tokens: None,
        operation_class: None,
        response_shape: None,
        prompt_mode: None,
        facts: None,
        max_output_tokens: 64,
    };
    match sampler.sample(tool, CancellationToken::new()).await {
        Ok(sample) => {
            eprintln!(
                "L3 probe OK [ToolResultSummary / DIRECT-CALL CAPABILITY ONLY — \
                 not a production lane under DERIV-12]: model={} label={} text_len={}",
                sample.model,
                sample.prompt_label,
                sample.text.len()
            );
            assert_eq!(
                sample.model, "grok-4.5",
                "ToolResultSummary capability probe must use grok-4.5"
            );
        }
        Err(err) => {
            return Err(format!(
                "BLOCKED: ToolResultSummary direct-call capability probe failed \
                 ({:?}: {}). This is a sampler reachability check, not proof of \
                 a production drain lane (DERIV-12).",
                err.kind, err.detail
            ));
        }
    }
    let _ = LhcInferenceOp::SmoothPrompt;
    Ok("prompt-smoothing-production-ok+tool-result-capability-probe-ok".into())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "LIVE_RUNBOOK L1 — needs ~/.grok auth + multi-minute real grok-4.5 drain; run with --ignored"]
async fn l3_g2_real_inference_writeback_body_vs_fixture() {
    let auth_path = grok_home().join("auth.json");
    if !auth_path.is_file() {
        panic!(
            "BLOCKED: no {} — cannot run real-inference G2. \
             Tried: look for ~/.grok/auth.json. No shim substituted.",
            auth_path.display()
        );
    }

    let am = Arc::new(AuthManager::new(&grok_home(), GrokComConfig::default()));
    let auth = am.auth().await.unwrap_or_else(|e| {
        panic!(
            "BLOCKED: AuthManager::auth failed ({e}). \
             Tried: load ~/.grok/auth.json via AuthManager. No shim substituted."
        )
    });
    let base_url = EndpointsConfig::default().resolve_inference_base_url();
    let cfg = SamplerConfig {
        api_key: Some(auth.key.clone()),
        base_url: base_url.clone(),
        model: "session-must-not-be-used".into(),
        context_window: 128_000,
        client_version: Some(xai_grok_version::VERSION.to_string()),
        ..Default::default()
    };
    let sampler = ShellLhcInferenceSampler::new(
        cfg,
        Some(am.clone()),
        "l3-real-g2",
        Duration::from_secs(180),
    );

    if let Err(msg) = probe_real_sampler(&sampler).await {
        panic!("{msg}");
    }

    let root = TempDir::new().unwrap();
    let prev_lhc = std::env::var_os("GROK_LHC");
    let prev_root = std::env::var_os("GROK_LHC_ROOT");
    let prev_c = std::env::var_os("GROK_LHC_COMPACT");
    let prev_e = std::env::var_os("GROK_LHC_COMPACT_EXPERIMENTAL");
    let prev_m = std::env::var_os("GROK_LHC_INFERENCE_MODEL");
    unsafe {
        std::env::set_var("GROK_LHC", "1");
        std::env::set_var("GROK_LHC_ROOT", root.path());
        std::env::set_var("GROK_LHC_COMPACT", "replace");
        std::env::set_var("GROK_LHC_COMPACT_EXPERIMENTAL", "1");
        std::env::remove_var("GROK_LHC_INFERENCE_MODEL");
    }

    let sid = format!(
        "l3-g2-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let native = multi_turn_native(SEED_TURNS, SEED_WORDS_PER_MSG);
    let seed_tokens = estimated_seed_tokens(&native);
    eprintln!(
        "L3 G2 seed: turns={SEED_TURNS} words/msg={SEED_WORDS_PER_MSG} \
         est_tokens={seed_tokens} prod_full_budget={PROD_FULL_BUDGET} \
         (continuation lower_bound={PROD_LOWER_BOUND} full%={PROD_FULL_PERCENT})"
    );
    eprintln!(
        "L3 G2 NOTE: seed is one history batch at spawn_capture — NOT paced \
         turns with Background gaps. L1 does not prove between-turn settling \
         (see LIVE_RUNBOOK)."
    );
    if (seed_tokens as f64) < PROD_FULL_BUDGET {
        restore(prev_lhc, prev_root, prev_c, prev_e, prev_m);
        panic!(
            "M1 seed too small for production banding: est_tokens={seed_tokens} \
             < full_budget={PROD_FULL_BUDGET}. Enlarge SEED_TURNS/SEED_WORDS_PER_MSG; \
             do not shrink ViewCompactParams."
        );
    }

    let capture = spawn_capture(
        &sid,
        Some("/tmp"),
        &native,
        Some(root.path()),
        Some(sampler.into_arc()),
    )
    .expect("capture with real sampler");

    // Compact is selection + fallback ladder — never drains. Soft ceiling
    // catches architecture regressions (drain wrongly back on compact).
    let compact_budget = Duration::from_secs(120);
    let compact_started = Instant::now();
    let wb = match tokio::time::timeout(compact_budget, replace_compact_for_writeback(&sid)).await {
        Ok(Ok(wb)) => {
            let wall = compact_started.elapsed();
            eprintln!(
                "L3 G2: replace_compact_for_writeback OK in {:.1}s \
                 (selection-only; compact never drains)",
                wall.as_secs_f64()
            );
            assert!(
                wall < COMPACT_FAST_CEILING,
                "architecture regression: compact took {wall:?} (≥ {COMPACT_FAST_CEILING:?}). \
                 Background drain should leave selection-only compact in fractions of a \
                 second when ready≈total; hundreds of seconds means drain is back on compact."
            );
            wb
        }
        Ok(Err(err)) => {
            capture.shutdown_async();
            tokio::time::sleep(Duration::from_millis(200)).await;
            restore(prev_lhc, prev_root, prev_c, prev_e, prev_m);
            panic!("BLOCKED: replace_compact_for_writeback failed: {err}");
        }
        Err(_) => {
            capture.shutdown_async();
            tokio::time::sleep(Duration::from_millis(200)).await;
            restore(prev_lhc, prev_root, prev_c, prev_e, prev_m);
            panic!(
                "BLOCKED / impractical: replace_compact_for_writeback (selection-only) \
                 exceeded {}s for seed turns={SEED_TURNS} words/msg={SEED_WORDS_PER_MSG} \
                 est_tokens={seed_tokens} full_budget={PROD_FULL_BUDGET}. \
                 Do not shrink ViewCompactParams — report to live track.",
                compact_budget.as_secs()
            );
        }
    };
    eprintln!(
        "L3 G2 real-inference: view entries={} receipt={} degraded={} gaps={}",
        wb.view.entries.len(),
        wb.receipt_total_tokens,
        wb.receipt.degraded.len(),
        wb.receipt.gaps.len()
    );
    let body = build_writeback_conversation(&native, &wb.view, &wb.kinds)
        .expect("writeback body from real compact");
    let bands = band_count(&body);
    let real_fp = body_fingerprint(&body);
    let fixture_fp = adapter_fixture_fingerprint();

    // Keep capture alive only until body is built; gates open their own workers.
    capture.shutdown_async();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        bands > 0,
        "M1 FAIL: production params:None compact produced bands=0 on a seed that \
         should exceed full_budget. est_tokens={seed_tokens} full_budget={PROD_FULL_BUDGET} \
         turns={SEED_TURNS} words/msg={SEED_WORDS_PER_MSG} body_len={} receipt={}. \
         Refusing to compare an uncompacted body to the fixture.",
        body.len(),
        wb.receipt_total_tokens
    );

    // Degraded rungs are designed (02-domain-design) — report + receipt-check.
    report_degraded(&body, &wb.receipt);

    let kind_matched = real_fp
        .lines()
        .zip(fixture_fp.lines())
        .filter(|(a, b)| a.split(':').next() == b.split(':').next())
        .count();
    let fingerprint_match = real_fp == fixture_fp;
    eprintln!(
        "=== L3 G2 real write-back body ({} items, bands={bands}) ===\n{real_fp}",
        body.len()
    );
    eprintln!("=== L3 G2 adapter fixture (kind sketch) ===\n{fixture_fp}");
    if fingerprint_match {
        eprintln!(
            "L3 G2: real compacted body MATCHES adapter fixture fingerprint \
             (items={} bands={bands})",
            body.len()
        );
    } else {
        eprintln!(
            "L3 G2 FINDING: real compacted body ({} items, bands={bands}) DIFFERS from \
             adapter fixture sketch (9 items). kind-prefix overlap≈{kind_matched}. \
             Classify before any fixture change (see FORK.md / LIVE_RUNBOOK.md): \
             expected compaction variance | different input coverage | calibration error.",
            body.len()
        );
    }

    // Five hard gates + equivalence on the **credentialed** body.
    // Async variant: sync `run_five_gates_on_body` uses blocking_send and
    // panics inside a Tokio runtime (R2).
    eprintln!(
        "L3 G2: running five hard gates + B8.3 on body_len={} bands={bands} \
         label=credentialed-real-inference-body",
        body.len()
    );
    run_five_gates_on_body_async(
        &format!("{sid}-gates"),
        &native,
        &body,
        root.path(),
        "credentialed-real-inference-body",
    )
    .await;
    reset_equivalence_counters();
    let cal = compare_serve_equivalence(&body, &body);
    assert!(
        !cal.structural_divergence && !cal.informational_divergence,
        "credentialed-body self-compare must be silent"
    );
    let _ = project_conversation_canonical(&body);
    let obs = observe_serve_equivalence(&sid, Some(0), true, true, &body, &body);
    assert!(
        obs.compared,
        "credentialed-body equivalence calibration must compare"
    );
    let snap = equivalence_snapshot();
    eprintln!(
        "B8.3 calibration (credentialed-real-inference-body): compared={} fallen_back={} \
         structural={} informational={} ratio={}:{}",
        snap.turns_served_and_compared,
        snap.turns_fallen_back,
        snap.structural_divergences,
        snap.informational_divergences,
        snap.turns_fallen_back,
        snap.turns_served_and_compared
    );
    assert!(snap.turns_served_and_compared > 0);

    restore(prev_lhc, prev_root, prev_c, prev_e, prev_m);
}

fn restore(
    prev_lhc: Option<std::ffi::OsString>,
    prev_root: Option<std::ffi::OsString>,
    prev_c: Option<std::ffi::OsString>,
    prev_e: Option<std::ffi::OsString>,
    prev_m: Option<std::ffi::OsString>,
) {
    match prev_lhc {
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
    match prev_m {
        Some(v) => unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", v) },
        None => unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") },
    }
}
