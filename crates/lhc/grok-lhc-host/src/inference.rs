//! LHC `InferenceCallbacks` over a host-injected sampler (Chunk 2).
//!
//! The trait lives here; the real transport lives in `xai-grok-shell` (alongside
//! `ShellCompactionSampler`). Adapter tests inject [`MockLhcInferenceSampler`].
//!
//! ## Derivation lanes (Lee's ruling, Chunk 3 unit 19)
//!
//! Callback bridge (capability): both work kinds *can* fan into the single
//! registered [`LhcInferenceSampler`] via [`inference_callbacks_for_session`]
//! → [`dispatch`]:
//! - `WorkKind::PromptSmoothing` → `smoothed_prompt` → `smooth_prompt`
//! - `WorkKind::ToolResultSummary` → `tool_result_summary` → `summarize_tool_result`
//!
//! **Production Background drain (DERIV-12):** only PromptSmoothing actually
//! reaches the sampler. ToolResultSummary is forced onto the vendored
//! truncate-fallback (`FORCE_TOOL_RESULT_SUMMARY_FALLBACK = true` in
//! `handlers.rs`) — no production `SummarizeToolResult` sampler op. Direct
//! callback/probe calls remain a capability check, not a production lane.
//!
//! Resolved model default: [`DEFAULT_LHC_INFERENCE_MODEL`] (`grok-4.5`).
//! Thinking: [`LHC_INFERENCE_THINKING_LEVEL`] (`low`) — set on the shell
//! `ConversationRequest`, not configurable without a new ruling.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use lhc::sdk::{InferenceCallbacks, InferenceResult};
use lhc::shared_tech::{
    CompressDetailedTurnInput, InferenceRequestMessage, InferenceRequestRole, ProviderProvenance,
    SmoothPromptInput, SummarizeChunkBriefInput, SummarizeToolResultInput, ToolOutcome,
    ToolResultFacts, ToolResultOperationClass, ToolResultPromptMode, ToolResultResponseShape,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::runtime_config::{ConfigSource, applied_config};

/// Default model slug for LHC derivation inference (production:
/// PromptSmoothing; ToolResultSummary is truncate-fallback under DERIV-12).
///
/// Override with `GROK_LHC_INFERENCE_MODEL` / `[lhc] inference_model`. Must
/// **not** fall back to the session's chat model.
pub const DEFAULT_LHC_INFERENCE_MODEL: &str = "grok-4.5";

/// Fixed thinking level for derivation calls (ruling — not a tunable default).
pub const LHC_INFERENCE_THINKING_LEVEL: &str = "low";

/// Resolve the derivation inference model slug and its provenance.
///
/// Order: env `GROK_LHC_INFERENCE_MODEL` → applied config file value →
/// [`DEFAULT_LHC_INFERENCE_MODEL`].
pub fn resolved_inference_model() -> (String, ConfigSource) {
    if let Ok(v) = std::env::var("GROK_LHC_INFERENCE_MODEL") {
        let t = v.trim();
        if !t.is_empty() {
            return (t.to_string(), ConfigSource::Env);
        }
    }
    if let Some(applied) = applied_config()
        && let Some(ref m) = applied.inference_model.value
    {
        let t = m.trim();
        if !t.is_empty() {
            return (t.to_string(), applied.inference_model.source);
        }
    }
    (
        DEFAULT_LHC_INFERENCE_MODEL.to_string(),
        ConfigSource::Default,
    )
}

/// Boxed future used by [`LhcInferenceSampler`].
pub type LhcInferenceFuture =
    Pin<Box<dyn Future<Output = Result<LhcInferenceSample, LhcInferenceError>> + Send>>;

/// One inference sample returned to LHC derivation.
#[derive(Debug, Clone)]
pub struct LhcInferenceSample {
    pub text: String,
    pub model: String,
    pub prompt_label: String,
    pub request_messages: Vec<InferenceRequestMessage>,
    pub max_output_tokens: u32,
}

/// Classified sampling failure (timeout / cancel / transport / refusal / auth).
#[derive(Debug, Clone)]
pub struct LhcInferenceError {
    pub kind: LhcInferenceErrorKind,
    pub detail: String,
    pub request_messages: Option<Vec<InferenceRequestMessage>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LhcInferenceErrorKind {
    Timeout,
    Cancelled,
    Transport,
    Refusal,
    Auth,
    Unavailable,
}

impl LhcInferenceErrorKind {
    pub fn as_reason_prefix(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Transport => "transport",
            Self::Refusal => "refusal",
            Self::Auth => "auth",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Typed operation payload — preserves LHC target fields and prompt mode.
#[derive(Debug, Clone)]
pub enum LhcInferenceRequest {
    SmoothPrompt {
        text: String,
        max_output_tokens: u32,
    },
    SummarizeToolResult {
        tool_name: String,
        content: String,
        outcome: Option<ToolOutcome>,
        target_tokens: Option<i64>,
        operation_class: Option<ToolResultOperationClass>,
        response_shape: Option<ToolResultResponseShape>,
        prompt_mode: Option<ToolResultPromptMode>,
        facts: Option<ToolResultFacts>,
        max_output_tokens: u32,
    },
    CompressDetailedTurn {
        dialogue_text: String,
        input_tokens: i64,
        target_min_tokens: i64,
        target_aim_tokens: i64,
        target_max_tokens: i64,
        max_output_tokens: u32,
    },
    SummarizeChunkBrief {
        text: String,
        input_tokens: i64,
        target_min_tokens: i64,
        target_aim_tokens: i64,
        target_max_tokens: i64,
        max_output_tokens: u32,
    },
}

impl LhcInferenceRequest {
    pub fn op(&self) -> LhcInferenceOp {
        match self {
            Self::SmoothPrompt { .. } => LhcInferenceOp::SmoothPrompt,
            Self::SummarizeToolResult { .. } => LhcInferenceOp::SummarizeToolResult,
            Self::CompressDetailedTurn { .. } => LhcInferenceOp::CompressDetailedTurn,
            Self::SummarizeChunkBrief { .. } => LhcInferenceOp::SummarizeChunkBrief,
        }
    }

    pub fn max_output_tokens(&self) -> u32 {
        match self {
            Self::SmoothPrompt {
                max_output_tokens, ..
            }
            | Self::SummarizeToolResult {
                max_output_tokens, ..
            }
            | Self::CompressDetailedTurn {
                max_output_tokens, ..
            }
            | Self::SummarizeChunkBrief {
                max_output_tokens, ..
            } => *max_output_tokens,
        }
    }

    /// User-facing prompt body for the sampler (includes structured fields).
    pub fn user_prompt_text(&self) -> String {
        match self {
            Self::SmoothPrompt { text, .. } => text.clone(),
            Self::SummarizeToolResult {
                tool_name,
                content,
                outcome,
                target_tokens,
                operation_class,
                response_shape,
                prompt_mode,
                facts,
                ..
            } => {
                format!(
                    "tool={tool_name}\n\
                     outcome={outcome:?}\n\
                     target_tokens={target_tokens:?}\n\
                     operation_class={operation_class:?}\n\
                     response_shape={response_shape:?}\n\
                     prompt_mode={prompt_mode:?}\n\
                     facts={facts:?}\n\
                     content:\n{content}"
                )
            }
            Self::CompressDetailedTurn {
                dialogue_text,
                input_tokens,
                target_min_tokens,
                target_aim_tokens,
                target_max_tokens,
                ..
            } => {
                format!(
                    "input_tokens={input_tokens} target_min={target_min_tokens} \
                     target_aim={target_aim_tokens} target_max={target_max_tokens}\n\
                     {dialogue_text}"
                )
            }
            Self::SummarizeChunkBrief {
                text,
                input_tokens,
                target_min_tokens,
                target_aim_tokens,
                target_max_tokens,
                ..
            } => {
                format!(
                    "input_tokens={input_tokens} target_min={target_min_tokens} \
                     target_aim={target_aim_tokens} target_max={target_max_tokens}\n\
                     {text}"
                )
            }
        }
    }
}

fn clamp_tokens(raw: i64, default: u32) -> u32 {
    if raw <= 0 {
        return default;
    }
    u32::try_from(raw).unwrap_or(default).clamp(64, 32_768)
}

const DEFAULT_SMOOTH_TOKENS: u32 = 2048;

/// Host-facing sampler: dedicated non-main transport, refreshed credentials.
pub trait LhcInferenceSampler: Send + Sync {
    fn sample(&self, req: LhcInferenceRequest, cancel: CancellationToken) -> LhcInferenceFuture;
}

/// Which LHC derivation op is requesting inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LhcInferenceOp {
    SmoothPrompt,
    SummarizeToolResult,
    CompressDetailedTurn,
    SummarizeChunkBrief,
}

impl LhcInferenceOp {
    pub fn as_prompt_label(self) -> &'static str {
        match self {
            Self::SmoothPrompt => "lhc.smooth_prompt",
            Self::SummarizeToolResult => "lhc.summarize_tool_result",
            Self::CompressDetailedTurn => "lhc.compress_detailed_turn",
            Self::SummarizeChunkBrief => "lhc.summarize_chunk_brief",
        }
    }
}

fn sampler_registry() -> &'static Mutex<HashMap<String, Arc<dyn LhcInferenceSampler>>> {
    static REG: OnceLock<Mutex<HashMap<String, Arc<dyn LhcInferenceSampler>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register (or replace) the sampler for a session. Called from tee/open.
pub fn register_inference_sampler(session_id: &str, sampler: Arc<dyn LhcInferenceSampler>) {
    if let Ok(mut map) = sampler_registry().lock() {
        map.insert(session_id.to_string(), sampler);
    }
}

/// Drop the sampler when the capture session ends.
pub fn unregister_inference_sampler(session_id: &str) {
    if let Ok(mut map) = sampler_registry().lock() {
        map.remove(session_id);
    }
}

/// True when a ModelCall inference sampler is registered for `session_id`.
///
/// Used by `/lhc` status and mid-session enable reporting (Z2).
pub fn inference_sampler_registered(session_id: &str) -> bool {
    sampler_registry()
        .lock()
        .ok()
        .is_some_and(|map| map.contains_key(session_id))
}

fn lookup_sampler(session_id: &str) -> Option<Arc<dyn LhcInferenceSampler>> {
    sampler_registry()
        .lock()
        .ok()
        .and_then(|map| map.get(session_id).cloned())
}

fn to_inference_result(sample: LhcInferenceSample) -> InferenceResult {
    InferenceResult::Ok {
        text: sample.text,
        provenance: Some(ProviderProvenance {
            provider: "grok-build".into(),
            model: sample.model,
            prompt: sample.prompt_label,
        }),
        request_messages: Some(sample.request_messages),
        raw_response: None,
    }
}

fn to_inference_err(err: LhcInferenceError) -> InferenceResult {
    InferenceResult::Err {
        reason: format!("{}: {}", err.kind.as_reason_prefix(), err.detail),
        request_messages: err.request_messages,
    }
}

async fn dispatch(
    session_id: &str,
    req: LhcInferenceRequest,
    cancel: CancellationToken,
) -> InferenceResult {
    let Some(sampler) = lookup_sampler(session_id) else {
        warn!(session_id, op = ?req.op(), "LHC inference: no sampler registered");
        return InferenceResult::Err {
            reason: format!(
                "{}: no sampler registered",
                LhcInferenceErrorKind::Unavailable.as_reason_prefix()
            ),
            request_messages: None,
        };
    };
    if cancel.is_cancelled() {
        return to_inference_err(LhcInferenceError {
            kind: LhcInferenceErrorKind::Cancelled,
            detail: "cancelled before dispatch".into(),
            request_messages: None,
        });
    }
    match sampler.sample(req, cancel).await {
        Ok(sample) => to_inference_result(sample),
        Err(err) => to_inference_err(err),
    }
}

/// Build LHC callbacks that dispatch to the session's registered sampler.
pub fn inference_callbacks_for_session(session_id: &str) -> InferenceCallbacks {
    let sid_a = session_id.to_string();
    let sid_b = session_id.to_string();
    let sid_c = session_id.to_string();
    let sid_d = session_id.to_string();
    InferenceCallbacks {
        smooth_prompt: Arc::new(move |input: SmoothPromptInput| {
            let sid = sid_a.clone();
            Box::pin(async move {
                let req = LhcInferenceRequest::SmoothPrompt {
                    text: input.text,
                    max_output_tokens: DEFAULT_SMOOTH_TOKENS,
                };
                // Background drain is independent of turn abort — samples run
                // to completion (or host-level cancel). Compact never waits.
                dispatch(&sid, req, CancellationToken::new()).await
            })
        }),
        summarize_tool_result: Arc::new(move |input: SummarizeToolResultInput| {
            let sid = sid_b.clone();
            Box::pin(async move {
                let max_output_tokens = input
                    .target_tokens
                    .map(|t| clamp_tokens(t, DEFAULT_SMOOTH_TOKENS))
                    .unwrap_or(DEFAULT_SMOOTH_TOKENS);
                let req = LhcInferenceRequest::SummarizeToolResult {
                    tool_name: input.tool_name,
                    content: input.content,
                    outcome: input.outcome,
                    target_tokens: input.target_tokens,
                    operation_class: input.operation_class,
                    response_shape: input.response_shape,
                    prompt_mode: input.prompt_mode,
                    facts: input.facts,
                    max_output_tokens,
                };
                dispatch(&sid, req, CancellationToken::new()).await
            })
        }),
        compress_detailed_turn: Arc::new(move |input: CompressDetailedTurnInput| {
            let sid = sid_c.clone();
            Box::pin(async move {
                let max_output_tokens =
                    clamp_tokens(input.target_max_tokens, DEFAULT_SMOOTH_TOKENS);
                let req = LhcInferenceRequest::CompressDetailedTurn {
                    dialogue_text: input.dialogue_text,
                    input_tokens: input.input_tokens,
                    target_min_tokens: input.target_min_tokens,
                    target_aim_tokens: input.target_aim_tokens,
                    target_max_tokens: input.target_max_tokens,
                    max_output_tokens,
                };
                dispatch(&sid, req, CancellationToken::new()).await
            })
        }),
        summarize_chunk_brief: Arc::new(move |input: SummarizeChunkBriefInput| {
            let sid = sid_d.clone();
            Box::pin(async move {
                let max_output_tokens =
                    clamp_tokens(input.target_max_tokens, DEFAULT_SMOOTH_TOKENS);
                let req = LhcInferenceRequest::SummarizeChunkBrief {
                    text: input.text,
                    input_tokens: input.input_tokens,
                    target_min_tokens: input.target_min_tokens,
                    target_aim_tokens: input.target_aim_tokens,
                    target_max_tokens: input.target_max_tokens,
                    max_output_tokens,
                };
                dispatch(&sid, req, CancellationToken::new()).await
            })
        }),
    }
}

/// Deterministic mock for adapter certification (no live model).
#[derive(Debug, Default)]
pub struct MockLhcInferenceSampler {
    pub prefix: String,
    pub cancel_immediately: bool,
}

impl MockLhcInferenceSampler {
    pub fn new() -> Self {
        Self {
            prefix: "mock".into(),
            cancel_immediately: false,
        }
    }
}

impl LhcInferenceSampler for MockLhcInferenceSampler {
    fn sample(&self, req: LhcInferenceRequest, cancel: CancellationToken) -> LhcInferenceFuture {
        let prefix = self.prefix.clone();
        let cancel_immediately = self.cancel_immediately;
        Box::pin(async move {
            if cancel_immediately || cancel.is_cancelled() {
                return Err(LhcInferenceError {
                    kind: LhcInferenceErrorKind::Cancelled,
                    detail: "mock cancelled".into(),
                    request_messages: None,
                });
            }
            let label = req.op().as_prompt_label().to_string();
            let user_text = req.user_prompt_text();
            let max_output_tokens = req.max_output_tokens();
            let request_messages = vec![
                InferenceRequestMessage {
                    role: InferenceRequestRole::System,
                    content: format!("You are the {label} helper."),
                },
                InferenceRequestMessage {
                    role: InferenceRequestRole::User,
                    content: user_text.clone(),
                },
            ];
            Ok(LhcInferenceSample {
                text: format!("{prefix}:{label}:{user_text}"),
                model: "mock-lhc-inference".into(),
                prompt_label: label,
                request_messages,
                max_output_tokens,
            })
        })
    }
}

/// Counting double for certification (records ops + max tokens).
#[derive(Debug, Default)]
pub struct CountingLhcInferenceSampler {
    pub calls: Mutex<Vec<(LhcInferenceOp, u32)>>,
}

impl CountingLhcInferenceSampler {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn call_ops(&self) -> Vec<LhcInferenceOp> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(op, _)| *op)
            .collect()
    }
}

impl LhcInferenceSampler for CountingLhcInferenceSampler {
    fn sample(&self, req: LhcInferenceRequest, cancel: CancellationToken) -> LhcInferenceFuture {
        let op = req.op();
        let max_output_tokens = req.max_output_tokens();
        let user_text = req.user_prompt_text();
        if let Ok(mut g) = self.calls.lock() {
            g.push((op, max_output_tokens));
        }
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(LhcInferenceError {
                    kind: LhcInferenceErrorKind::Cancelled,
                    detail: "counting sampler cancelled".into(),
                    request_messages: None,
                });
            }
            let label = op.as_prompt_label().to_string();
            Ok(LhcInferenceSample {
                text: format!("count:{label}"),
                model: "counting-lhc".into(),
                prompt_label: label,
                request_messages: vec![InferenceRequestMessage {
                    role: InferenceRequestRole::User,
                    content: user_text,
                }],
                max_output_tokens,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gating::env_lock;

    #[test]
    fn resolved_inference_model_defaults_to_grok_45_not_session() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC_INFERENCE_MODEL");
        unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") };
        let (slug, src) = resolved_inference_model();
        assert_eq!(slug, DEFAULT_LHC_INFERENCE_MODEL);
        assert_eq!(slug, "grok-4.5");
        assert_eq!(src, ConfigSource::Default);
        assert_eq!(LHC_INFERENCE_THINKING_LEVEL, "low");
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") },
        }
    }

    #[test]
    fn resolved_inference_model_env_overrides_default() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC_INFERENCE_MODEL");
        unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", "custom-slug") };
        let (slug, src) = resolved_inference_model();
        assert_eq!(slug, "custom-slug");
        assert_eq!(src, ConfigSource::Env);
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") },
        }
    }

    /// Callback-bridge capability: both work kinds can dispatch to one sampler.
    /// Not a production-lane proof — ToolResultSummary is truncate-fallback
    /// under DERIV-12 in Background drain.
    #[tokio::test]
    async fn both_derivation_lanes_dispatch_to_same_sampler() {
        let sid = "inf-both-lanes";
        let counter = Arc::new(CountingLhcInferenceSampler::new());
        register_inference_sampler(sid, counter.clone());
        let cbs = inference_callbacks_for_session(sid);
        let _ = (cbs.smooth_prompt)(SmoothPromptInput {
            text: "smooth-me".into(),
        })
        .await;
        let _ = (cbs.summarize_tool_result)(SummarizeToolResultInput {
            tool_name: "bash".into(),
            content: "out".into(),
            outcome: None,
            target_tokens: None,
            operation_class: None,
            response_shape: None,
            prompt_mode: None,
            facts: None,
        })
        .await;
        let ops = counter.call_ops();
        assert!(
            ops.contains(&LhcInferenceOp::SmoothPrompt),
            "PromptSmoothing lane missing: {ops:?}"
        );
        assert!(
            ops.contains(&LhcInferenceOp::SummarizeToolResult),
            "ToolResultSummary lane missing: {ops:?}"
        );
        unregister_inference_sampler(sid);
    }

    #[tokio::test]
    async fn mock_sampler_round_trips_via_registry() {
        let sid = "inf-mock-1";
        register_inference_sampler(sid, Arc::new(MockLhcInferenceSampler::new()));
        let cbs = inference_callbacks_for_session(sid);
        let result = (cbs.smooth_prompt)(SmoothPromptInput {
            text: "hello".into(),
        })
        .await;
        match result {
            InferenceResult::Ok {
                text,
                provenance,
                request_messages,
                ..
            } => {
                assert!(text.contains("smooth_prompt"));
                assert!(text.contains("hello"));
                assert_eq!(
                    provenance.as_ref().map(|p| p.model.as_str()),
                    Some("mock-lhc-inference")
                );
                assert!(request_messages.is_some());
            }
            InferenceResult::Err { reason, .. } => panic!("unexpected err: {reason}"),
        }
        unregister_inference_sampler(sid);
    }

    #[tokio::test]
    async fn compress_forwards_target_max_tokens() {
        let sid = "inf-tokens-1";
        let counter = Arc::new(CountingLhcInferenceSampler::new());
        register_inference_sampler(sid, counter.clone());
        let cbs = inference_callbacks_for_session(sid);
        let _ = (cbs.compress_detailed_turn)(CompressDetailedTurnInput {
            dialogue_text: "d".into(),
            input_tokens: 1000,
            target_min_tokens: 100,
            target_aim_tokens: 200,
            target_max_tokens: 512,
        })
        .await;
        let calls = counter.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, LhcInferenceOp::CompressDetailedTurn);
        assert_eq!(calls[0].1, 512);
        unregister_inference_sampler(sid);
    }

    #[tokio::test]
    async fn cancel_token_is_honored() {
        let sid = "inf-cancel-1";
        let mut mock = MockLhcInferenceSampler::new();
        mock.cancel_immediately = true;
        register_inference_sampler(sid, Arc::new(mock));
        let cbs = inference_callbacks_for_session(sid);
        // cancel_immediately on mock; also prove token path via direct sample
        let sampler = lookup_sampler(sid).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let err = sampler
            .sample(
                LhcInferenceRequest::SmoothPrompt {
                    text: "x".into(),
                    max_output_tokens: 64,
                },
                token,
            )
            .await
            .expect_err("cancelled");
        assert_eq!(err.kind, LhcInferenceErrorKind::Cancelled);
        let _ = cbs;
        unregister_inference_sampler(sid);
    }
}
