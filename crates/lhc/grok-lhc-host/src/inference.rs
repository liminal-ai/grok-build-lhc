//! LHC `InferenceCallbacks` over a host-injected sampler (Chunk 2).
//!
//! The trait lives here; the real transport lives in `xai-grok-shell` (alongside
//! `ShellCompactionSampler`). Adapter tests inject [`MockLhcInferenceSampler`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use lhc::sdk::{InferenceCallbacks, InferenceResult};
use lhc::shared_tech::{
    CompressDetailedTurnInput, InferenceRequestMessage, InferenceRequestRole, ProviderProvenance,
    SmoothPromptInput, SummarizeChunkBriefInput, SummarizeToolResultInput,
};
use tracing::warn;

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
}

/// Classified sampling failure (timeout / cancel / transport / refusal).
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
    Unavailable,
}

impl LhcInferenceErrorKind {
    pub fn as_reason_prefix(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Transport => "transport",
            Self::Refusal => "refusal",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Host-facing sampler: dedicated non-main model, same shape as compaction.
pub trait LhcInferenceSampler: Send + Sync {
    fn sample(&self, op: LhcInferenceOp, user_text: String) -> LhcInferenceFuture;
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

async fn dispatch(session_id: &str, op: LhcInferenceOp, user_text: String) -> InferenceResult {
    let Some(sampler) = lookup_sampler(session_id) else {
        warn!(session_id, op = ?op, "LHC inference: no sampler registered");
        return InferenceResult::Err {
            reason: format!(
                "{}: no sampler registered",
                LhcInferenceErrorKind::Unavailable.as_reason_prefix()
            ),
            request_messages: None,
        };
    };
    match sampler.sample(op, user_text).await {
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
            Box::pin(async move { dispatch(&sid, LhcInferenceOp::SmoothPrompt, input.text).await })
        }),
        summarize_tool_result: Arc::new(move |input: SummarizeToolResultInput| {
            let sid = sid_b.clone();
            Box::pin(async move {
                let text = format!("tool={} content={}", input.tool_name, input.content);
                dispatch(&sid, LhcInferenceOp::SummarizeToolResult, text).await
            })
        }),
        compress_detailed_turn: Arc::new(move |input: CompressDetailedTurnInput| {
            let sid = sid_c.clone();
            Box::pin(async move {
                dispatch(
                    &sid,
                    LhcInferenceOp::CompressDetailedTurn,
                    input.dialogue_text,
                )
                .await
            })
        }),
        summarize_chunk_brief: Arc::new(move |input: SummarizeChunkBriefInput| {
            let sid = sid_d.clone();
            Box::pin(async move {
                dispatch(&sid, LhcInferenceOp::SummarizeChunkBrief, input.text).await
            })
        }),
    }
}

/// Deterministic mock for adapter certification (no live model).
#[derive(Debug, Default)]
pub struct MockLhcInferenceSampler {
    pub prefix: String,
}

impl MockLhcInferenceSampler {
    pub fn new() -> Self {
        Self {
            prefix: "mock".into(),
        }
    }
}

impl LhcInferenceSampler for MockLhcInferenceSampler {
    fn sample(&self, op: LhcInferenceOp, user_text: String) -> LhcInferenceFuture {
        let prefix = self.prefix.clone();
        let label = op.as_prompt_label().to_string();
        Box::pin(async move {
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
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
