//! Chunk 3B / unit 19 — grok-4.5 derivation **quality** (S1) and **timing** (S2).
//!
//! Measurement only — no prompt/threshold/model tuning.
//!
//! Important port fact: PromptSmoothing **skips inference** when
//! `cleaned_tokens > max_inference_tokens` (default **700**). The G2 5000-word
//! seed never exercises grok-4.5 for smoothing; this harness uses ~400–600 token
//! identifier-rich prompts so the model is actually called.
//!
//! ```text
//! cargo test -p xai-grok-shell --lib lhc_derivation_quality_timing -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grok_lhc_host::{
    CaptureHandle, CompactMode, LhcInferenceFuture, LhcInferenceOp, LhcInferenceRequest,
    LhcInferenceSampler, build_writeback_conversation, estimate_tokens,
    replace_compact_for_writeback, set_compact_mode_for_test, set_compact_params_override_for_test,
    spawn_capture, thread_file_path,
};
use lhc::create_deterministic_inference_callbacks;
use lhc::init_lhc;
use lhc::intake_stream::MessageEventInput;
use lhc::shared_tech::derivation::{SdkConfig, SdkMode};
use lhc::shared_tech::errors::OpResult;
use lhc::shared_tech::logging::{LogLevel, LogQuery};
use lhc::shared_tech::view::{PartialViewProfilePercentages, ViewCompactParams};
use lhc::threads::{NewThreadInput, ThreadRef};
use serde_json::{Map, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use xai_grok_sampling_types::ConversationItem;

use crate::agent::config::EndpointsConfig;
use crate::auth::{AuthManager, GrokComConfig};
use crate::session::lhc_inference::ShellLhcInferenceSampler;
use xai_grok_sampler::SamplerConfig;

#[derive(Clone, Debug)]
struct SampleRecord {
    op: LhcInferenceOp,
    latency: Duration,
    input_chars: usize,
    output_chars: usize,
    ok: bool,
}

struct TimedShellSampler {
    inner: Arc<ShellLhcInferenceSampler>,
    samples: Arc<Mutex<Vec<SampleRecord>>>,
}

impl TimedShellSampler {
    fn new(inner: ShellLhcInferenceSampler) -> Self {
        Self {
            inner: Arc::new(inner),
            samples: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn records(&self) -> Vec<SampleRecord> {
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl LhcInferenceSampler for TimedShellSampler {
    fn sample(&self, req: LhcInferenceRequest, cancel: CancellationToken) -> LhcInferenceFuture {
        let inner = Arc::clone(&self.inner);
        let samples = Arc::clone(&self.samples);
        let op = req.op();
        let input_chars = req.user_prompt_text().len();
        Box::pin(async move {
            let t0 = Instant::now();
            let result = inner.sample(req, cancel).await;
            let latency = t0.elapsed();
            let (ok, output_chars) = match &result {
                Ok(s) => (true, s.text.len()),
                Err(_) => (false, 0),
            };
            if let Ok(mut g) = samples.lock() {
                g.push(SampleRecord {
                    op,
                    latency,
                    input_chars,
                    output_chars,
                    ok,
                });
            }
            eprintln!(
                "LATENCY op={op:?} ms={:.1} in_chars={input_chars} out_chars={output_chars} ok={ok}",
                latency.as_secs_f64() * 1000.0
            );
            result
        })
    }
}

fn grok_home() -> PathBuf {
    dirs::home_dir().expect("HOME").join(".grok")
}

fn restore_env(
    prev_lhc: Option<std::ffi::OsString>,
    prev_root: Option<std::ffi::OsString>,
    prev_c: Option<std::ffi::OsString>,
    prev_e: Option<std::ffi::OsString>,
    prev_m: Option<std::ffi::OsString>,
) {
    unsafe {
        match prev_lhc {
            Some(v) => std::env::set_var("GROK_LHC", v),
            None => std::env::remove_var("GROK_LHC"),
        }
        match prev_root {
            Some(v) => std::env::set_var("GROK_LHC_ROOT", v),
            None => std::env::remove_var("GROK_LHC_ROOT"),
        }
        match prev_c {
            Some(v) => std::env::set_var("GROK_LHC_COMPACT", v),
            None => std::env::remove_var("GROK_LHC_COMPACT"),
        }
        match prev_e {
            Some(v) => std::env::set_var("GROK_LHC_COMPACT_EXPERIMENTAL", v),
            None => std::env::remove_var("GROK_LHC_COMPACT_EXPERIMENTAL"),
        }
        match prev_m {
            Some(v) => std::env::set_var("GROK_LHC_INFERENCE_MODEL", v),
            None => std::env::remove_var("GROK_LHC_INFERENCE_MODEL"),
        }
    }
}

/// Identifier-rich prompts sized under the 700-token inference ceiling so
/// grok-4.5 is actually invoked (not skipped to cleaned floor).
fn quality_prompts() -> Vec<&'static str> {
    vec![
        "Refactor `crates/lhc/grok-lhc-host/src/session.rs` so `LhcSession::compact` \
         never awaits derivation drain. Keep `CompactAbortSignal` live-read semantics. \
         Do not change the vendored port. File must stay under 900 lines. After the \
         change, `/lhc status` must still show AbandonedByCancel on turn abort. \
         Constraints: no seventh hook; vendor pin e582465 untouched; Background mode \
         stays. Identifiers that must survive smoothing: CompactAbortSignal, \
         DERIVATION_DRAIN_BEFORE_COMPACT (remove call sites only), session_id \
         cert-r1-signal-wired, and the path crates/lhc/vendor/long-horizon-context.",
        "Investigate why PromptSmoothing skipped inference on the G2 seed. The port \
         guard is `cleaned_tokens > max_inference_tokens` with default 700 at \
         packages/lhc-rs/src/shared_tech/inference_types.rs:232. The G2 seed used \
         6×5000-word messages (~60k tokens). Confirm whether SmoothPrompt sampler ops \
         on that path were absent by design. Output: (1) yes/no on skip, (2) the exact \
         constant name max_inference_tokens, (3) recommended seed size for a real \
         grok-4.5 smoothing measurement. Do not modify thresholds. Mention ticket \
         DERIV-12 and the file handlers.rs:169 for discard_reason.",
        "Implement a measurement harness `lhc_derivation_quality_timing` that prints \
         original vs smoothed_prompt pairs and queries the thread `log` table for \
         reason=suspicious_output_ratio. Session root under /tmp/lhc-quality-$USER. \
         Must use ShellLhcInferenceSampler with model grok-4.5 and ReasoningEffort::Low. \
         Fail the run if any smoothed_prompt row has discard_reason set. Preserve \
         identifiers: suspicious_output_ratio, floor_used, text_len probe, and \
         LiveRunbook L1. No prompt tuning.",
        "Given backlog first-touch: open a Manual-mode thread with 4 queued \
         PromptSmoothing items, then reopen with SdkMode::Background via spawn_capture. \
         Measure wall time until health.queue.queued==0 and health.queue.claimed==0. \
         Report ready vs total from inspect.health owners. Reference t3code: 97 \
         summaries pre-built, 0.4s compact. Paths: ~/.grok/auth.json for credentials; \
         thread file via thread_file_path(root, sid). Do not raise drain budgets.",
        "Compare per-call latency of grok-4.5 @ low thinking against Codex's \
         ~1.66 s/call on gpt-5.6-luna and pi-lhc's openai-codex/gpt-5.4-mini at \
         thinking none (04-host-pi-lhc.md:159). Produce min/median/p90/max for \
         SmoothPrompt and any CompressDetailedTurn / SummarizeChunkBrief ops that \
         fire. If p90 exceeds 8s, flag intake-rate risk (DERIV-12 precedent). Keep \
         the model slug grok-4.5; do not switch models in this measurement.",
        "Write the S1/S2 report section for Lee: verdict viable | viable-with-caveat | \
         not-viable for grok-4.5 derivation on this host. Include verbatim smoothed \
         pairs and [context · smooth] / [context · brief] band text. Cite hooks 6/6, \
         vendor e582465, and that FORCE_TOOL_RESULT_SUMMARY_FALLBACK remains true so \
         ToolResultSummary is not in this timing set. Identifiers: \
         credentialed-real-inference-body, CompactDrainOutcome, run_five_gates_on_body_async.",
    ]
}

fn message_text(blocks: &[lhc::messages::Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.content.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn pct(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return f64::NAN;
    }
    let idx = ((p / 100.0) * (sorted_ms.len() as f64 - 1.0)).round() as usize;
    sorted_ms[idx.min(sorted_ms.len() - 1)]
}

fn print_latency_dist(label: &str, samples: &[SampleRecord]) {
    let mut ms: Vec<f64> = samples
        .iter()
        .map(|s| s.latency.as_secs_f64() * 1000.0)
        .collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if ms.is_empty() {
        eprintln!("LATENCY_DIST {label}: n=0 (kind did not fire)");
        return;
    }
    eprintln!(
        "LATENCY_DIST {label}: n={} min={:.0}ms median={:.0}ms p90={:.0}ms max={:.0}ms \
         (refs: Codex≈1660ms/call; pi-lhc gpt-5.4-mini@none)",
        ms.len(),
        ms[0],
        pct(&ms, 50.0),
        pct(&ms, 90.0),
        ms[ms.len() - 1]
    );
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

async fn wait_queue_settle(
    handle: &CaptureHandle,
    timeout: Duration,
) -> (bool, Duration, i64, i64) {
    let t0 = Instant::now();
    loop {
        let h = handle.inspect_health().await.expect("health");
        if h.queue.queued == 0 && h.queue.claimed == 0 {
            return (true, t0.elapsed(), h.queue.queued, h.queue.claimed);
        }
        if t0.elapsed() > timeout {
            return (false, t0.elapsed(), h.queue.queued, h.queue.claimed);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn ev(kind: &str, payload: Map<String, serde_json::Value>, key: &str) -> MessageEventInput {
    MessageEventInput {
        event_kind: kind.into(),
        idempotency_key: Some(key.into()),
        actor: "grok".into(),
        harness: "quality-timing".into(),
        payload,
        extra: Map::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "LIVE measurement — needs ~/.grok auth + network; not a tripwire"]
async fn s1_s2_grok45_derivation_quality_and_timing() {
    let auth_path = grok_home().join("auth.json");
    if !auth_path.is_file() {
        panic!(
            "BLOCKED: no {} — cannot measure real grok-4.5",
            auth_path.display()
        );
    }
    let am = Arc::new(AuthManager::new(&grok_home(), GrokComConfig::default()));
    let auth = am
        .auth()
        .await
        .unwrap_or_else(|e| panic!("BLOCKED: auth failed: {e}"));
    let base_url = EndpointsConfig::default().resolve_inference_base_url();
    let cfg = SamplerConfig {
        api_key: Some(auth.key.clone()),
        base_url,
        model: "session-must-not-be-used".into(),
        context_window: 128_000,
        client_version: Some(xai_grok_version::VERSION.to_string()),
        ..Default::default()
    };
    let shell =
        ShellLhcInferenceSampler::new(cfg, Some(am), "quality-timing", Duration::from_secs(180));
    let timer_arc = Arc::new(TimedShellSampler::new(shell));

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
        "qual-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    // --- S2 first-touch: Manual backlog, then Background open ---
    let backlog_sid = format!("{sid}-backlog");
    let backlog_thread = thread_file_path(root.path(), &backlog_sid);
    let registry = root.path().join("registry.sqlite");
    std::fs::create_dir_all(backlog_thread.parent().unwrap()).unwrap();
    {
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
                file_path: backlog_thread.to_string_lossy().into_owned(),
                title: Some("first-touch".into()),
                cwd: None,
                registry_path: Some(registry.to_string_lossy().into_owned()),
            })
            .await;
        let OpResult::Ok { value: info } = created else {
            panic!("new_thread failed: {created:?}");
        };
        let ref_ = ThreadRef::file_path(info.file_path);
        let mut batch = Vec::new();
        for (i, p) in quality_prompts().iter().take(4).enumerate() {
            batch.push(ev(
                "user_prompt",
                json!({ "text": p }).as_object().cloned().unwrap(),
                &format!("bu{i}"),
            ));
            batch.push(ev(
                "assistant_text",
                json!({ "text": format!("ack {i}") })
                    .as_object()
                    .cloned()
                    .unwrap(),
                &format!("ba{i}"),
            ));
            batch.push(ev("turn_end", Map::new(), &format!("bte{i}")));
        }
        let sub = manual.intake_stream.message_events(ref_, &batch).await;
        assert!(matches!(sub, OpResult::Ok { .. }), "{sub:?}");
        drop(manual);
    }

    let ft_handle = spawn_capture(
        &backlog_sid,
        Some("/tmp"),
        &[],
        Some(root.path()),
        Some(timer_arc.clone() as Arc<dyn LhcInferenceSampler>),
    )
    .expect("background open backlog thread");
    let health_before = ft_handle.inspect_health().await.expect("health");
    eprintln!(
        "FIRST_TOUCH before_write queued={} claimed={}",
        health_before.queue.queued, health_before.queue.claimed
    );
    let ft_t0 = Instant::now();
    ft_handle.persist(&ConversationItem::user("first-touch trigger"));
    ft_handle.flush().await.expect("flush");
    let (settled, ft_elapsed, q, c) = wait_queue_settle(&ft_handle, Duration::from_secs(600)).await;
    let ft_health = ft_handle.inspect_health().await.expect("health");
    let (ft_ready, ft_total) = health_ready_total(&ft_health);
    eprintln!(
        "FIRST_TOUCH settle_ok={settled} wall={ft_elapsed:?} queued={q} claimed={c} \
         ready={ft_ready} total={ft_total} (since_trigger={:?})",
        ft_t0.elapsed()
    );
    ft_handle.shutdown().await.ok();

    // --- Main multi-turn session: between-turn queue + quality ---
    let prompts = quality_prompts();
    for (i, p) in prompts.iter().enumerate() {
        let tok = estimate_tokens(p);
        eprintln!(
            "SEED prompt[{i}] est_tokens={tok} chars={} (must be ≤700 for inference)",
            p.len()
        );
        assert!(
            tok <= 700,
            "prompt[{i}] est_tokens={tok} exceeds max_inference_tokens=700 — \
             would skip grok-4.5; shorten the prompt"
        );
    }

    let mut native = vec![ConversationItem::system(
        "You are measuring LHC derivation quality. Follow constraints exactly.",
    )];
    let handle = spawn_capture(
        &sid,
        Some("/tmp"),
        &native,
        Some(root.path()),
        Some(timer_arc.clone() as Arc<dyn LhcInferenceSampler>),
    )
    .expect("capture");

    let mut queue_trace: Vec<(usize, i64, i64, bool)> = Vec::new();
    for (t, prompt) in prompts.iter().enumerate() {
        let mut u = ConversationItem::user((*prompt).to_string());
        u.set_prompt_index(t);
        native.push(u.clone());
        handle.persist(&u);
        let a = ConversationItem::assistant(format!(
            "Acknowledged turn {t}. Will preserve CompactAbortSignal and e582465."
        ));
        native.push(a.clone());
        handle.persist(&a);
        handle.flush().await.expect("flush");

        let sample_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < sample_deadline {
            let h = handle.inspect_health().await.expect("health");
            queue_trace.push((t, h.queue.queued, h.queue.claimed, false));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (ok, elapsed, q, c) = wait_queue_settle(&handle, Duration::from_secs(300)).await;
        queue_trace.push((t, q, c, ok));
        eprintln!(
            "BETWEEN_TURNS turn={t} settled={ok} wait={elapsed:?} final_queued={q} claimed={c}"
        );
    }

    let health_at_threshold = handle.inspect_health().await.expect("health");
    let (ready, total) = health_ready_total(&health_at_threshold);
    eprintln!(
        "READY_AT_COMPACT_TRIP ready={ready} total={total} queue={:?} \
         (healthy: ready≈total; t3code ref 97/97 then 0.4s compact)",
        health_at_threshold.queue
    );

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
    let compact_t0 = Instant::now();
    let wb = replace_compact_for_writeback(&sid).await.expect("compact");
    let compact_wall = compact_t0.elapsed();
    set_compact_mode_for_test(None);
    eprintln!(
        "COMPACT_WALL={compact_wall:?} receipt={} entries={}",
        wb.receipt_total_tokens,
        wb.view.entries.len()
    );

    let body = build_writeback_conversation(&native, &wb.view, &wb.kinds).expect("writeback body");
    eprintln!("=== BAND / BODY TEXT (verbatim) ===");
    for (i, item) in body.iter().enumerate() {
        let t = item.text_content();
        if t.contains("[context") || t.contains("[degraded") || t.contains("[gap") {
            eprintln!("--- body[{i}] ---\n{t}\n--- end body[{i}] ---");
        }
    }
    for item in &body {
        let t = item.text_content();
        if t.starts_with("[context · smooth]") || t.starts_with("[context · brief]") {
            eprintln!("BAND_VERBATIM:\n{t}\n");
        }
        if t.contains("[degraded:") {
            eprintln!("DEGRADED_BAND:\n{t}\n");
        }
    }

    handle.shutdown().await.ok();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let thread_path = thread_file_path(root.path(), &sid);
    let reader = init_lhc(SdkConfig {
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
    let tref = ThreadRef::file_path(thread_path.to_string_lossy().into_owned());

    let all_logs = match reader
        .logging
        .query(
            tref.clone(),
            LogQuery {
                level: None,
                derivation_type: None,
                subject_id: None,
                reason: None,
            },
        )
        .await
    {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => panic!("log query failed: {}", error.reason),
    };
    let suspicious: Vec<_> = all_logs
        .iter()
        .filter(|e| e.reason.as_deref() == Some("suspicious_output_ratio"))
        .collect();
    let floor_used_logs: Vec<_> = all_logs.iter().filter(|e| e.floor_used.is_some()).collect();
    eprintln!(
        "LOG_TABLE total={} suspicious_output_ratio={} floor_used_entries={} \
         (expected suspicious=0)",
        all_logs.len(),
        suspicious.len(),
        floor_used_logs.len()
    );
    for e in &suspicious {
        eprintln!(
            "  SUSPICIOUS log_id={} msg={} subject={:?} floor_len={}",
            e.log_id,
            e.message,
            e.subject_id,
            e.floor_used.as_ref().map(|s| s.len()).unwrap_or(0)
        );
    }
    for e in all_logs.iter().filter(|e| e.level == LogLevel::Warning) {
        eprintln!(
            "  WARN reason={:?} type={:?} msg={}",
            e.reason, e.derivation_type, e.message
        );
    }

    let msgs = match reader.messages.list(tref.clone(), None).await {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => panic!("messages.list failed: {}", error.reason),
    };
    let mut ratios: Vec<f64> = Vec::new();
    let mut discarded = 0usize;
    let mut inferred = 0usize;
    let mut skipped_no_inference = 0usize;
    eprintln!("=== SMOOTHED_PROMPT PAIRS (verbatim) ===");
    for m in &msgs {
        if m.kind != lhc::messages::MessageKind::UserPrompt {
            continue;
        }
        let original = message_text(&m.blocks);
        let Some(derivs) = &m.derivations else {
            continue;
        };
        for d in derivs {
            if d.derivation_type != "smoothed_prompt" {
                continue;
            }
            let smoothed = d.content.clone().unwrap_or_default();
            let in_tok = estimate_tokens(&original);
            let out_tok = estimate_tokens(&smoothed);
            let ratio = if in_tok > 0 {
                out_tok as f64 / in_tok as f64
            } else {
                f64::NAN
            };
            ratios.push(ratio);
            let discard = d.metadata.as_ref().and_then(|md| md.discard_reason.clone());
            if discard.is_some() {
                discarded += 1;
            }
            let attempted = d
                .metadata
                .as_ref()
                .and_then(|md| md.inference_attempted)
                .unwrap_or(false);
            if attempted {
                inferred += 1;
            } else {
                skipped_no_inference += 1;
            }
            eprintln!(
                "--- pair message_id={} in_tok={in_tok} out_tok={out_tok} ratio={ratio:.3} \
                 discard_reason={discard:?} inference_attempted={attempted} state={:?} ---",
                m.message_id, d.state
            );
            eprintln!("ORIGINAL:\n{original}\n");
            eprintln!("SMOOTHED:\n{smoothed}\n");
        }
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    eprintln!(
        "RATIO_DIST n={} discarded={discarded} inferred={inferred} \
         skipped_no_inference={skipped_no_inference} \
         ratios={ratios:?} (discard expected 0; ratio <0.15 triggers discard)",
        ratios.len()
    );

    let recs = timer_arc.records();
    let mut by_op: BTreeMap<String, Vec<SampleRecord>> = BTreeMap::new();
    for r in &recs {
        by_op
            .entry(format!("{:?}", r.op))
            .or_default()
            .push(r.clone());
    }
    eprintln!("SAMPLER_CALLS total={}", recs.len());
    for (op, rs) in &by_op {
        print_latency_dist(op, rs);
    }

    eprintln!("QUEUE_TRACE (turn, queued, claimed, settled_flag):");
    for (t, q, c, s) in &queue_trace {
        eprintln!("  turn={t} queued={q} claimed={c} settled={s}");
    }
    let settled_turns = queue_trace.iter().filter(|(_, _, _, s)| *s).count();
    let rising = queue_trace
        .windows(2)
        .filter(|w| w[1].1 > w[0].1 && !w[1].3)
        .count();
    eprintln!(
        "QUEUE_SUMMARY settled_samples={settled_turns}/{} rising_unsettled_steps={rising}",
        queue_trace.len()
    );

    restore_env(prev_lhc, prev_root, prev_c, prev_e, prev_m);

    assert!(
        inferred > 0,
        "S1: expected at least one SmoothPrompt inference call; got inferred={inferred} \
         skipped={skipped_no_inference}. Check prompt sizes vs max_inference_tokens=700."
    );
    eprintln!(
        "=== VERDICT INPUTS ready={ready}/{total} discarded={discarded} \
         suspicious_logs={} sampler_calls={} first_touch_settle={} ===",
        suspicious.len(),
        recs.len(),
        settled
    );
}
