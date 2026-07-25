//! Shell-side [`grok_lhc_host::LhcInferenceSampler`] — dedicated non-main model.
//!
//! Mirrors the compaction transport shape (`ShellCompactionSampler` /
//! `generate_session_compact`): builds a short conversation and collects a
//! completion. Runs on the session's aux/compaction model identity when
//! available; falls back to the primary model slug with a distinct req id.

use std::sync::Arc;
use std::time::Duration;

use grok_lhc_host::{
    InferenceRequestMessage, InferenceRequestRole, LhcInferenceError, LhcInferenceErrorKind,
    LhcInferenceFuture, LhcInferenceOp, LhcInferenceSample, LhcInferenceSampler,
};
use xai_grok_sampling_types::{ConversationItem, ConversationRequest};

use crate::sampling::Client as OaiCompatClient;
use crate::sampling::SamplerConfig;

/// Inference sampler wired at LHC tee/open (hook 2).
pub struct ShellLhcInferenceSampler {
    client: OaiCompatClient,
    model: String,
    session_id: String,
    timeout: Duration,
}

impl ShellLhcInferenceSampler {
    pub fn new(
        client: OaiCompatClient,
        model: String,
        session_id: String,
        timeout: Duration,
    ) -> Self {
        Self {
            client,
            model,
            session_id,
            timeout,
        }
    }

    /// Build from a sampler config (dedicated non-main / session model).
    pub fn from_config(
        config: SamplerConfig,
        session_id: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let model = config.model.clone();
        let client = OaiCompatClient::new(config).map_err(|e| e.to_string())?;
        Ok(Self::new(client, model, session_id.into(), timeout))
    }

    pub fn into_arc(self) -> Arc<dyn LhcInferenceSampler> {
        Arc::new(self)
    }
}

impl LhcInferenceSampler for ShellLhcInferenceSampler {
    fn sample(&self, op: LhcInferenceOp, user_text: String) -> LhcInferenceFuture {
        let client = self.client.clone();
        let model = self.model.clone();
        let session_id = self.session_id.clone();
        let timeout = self.timeout;
        let label = op.as_prompt_label().to_string();
        Box::pin(async move {
            let system = format!(
                "You are a Grok Build helper performing `{label}` for long-horizon context. \
                 Reply with only the transformed text, no preamble."
            );
            let request_messages = vec![
                InferenceRequestMessage {
                    role: InferenceRequestRole::System,
                    content: system.clone(),
                },
                InferenceRequestMessage {
                    role: InferenceRequestRole::User,
                    content: user_text.clone(),
                },
            ];
            let request = ConversationRequest {
                items: vec![
                    ConversationItem::system(system),
                    ConversationItem::user(user_text),
                ],
                tools: vec![],
                hosted_tools: vec![],
                tool_choice: None,
                model: Some(model.clone()),
                temperature: Some(0.0),
                max_output_tokens: Some(2048),
                top_p: None,
                x_grok_conv_id: Some(format!("lhc-inf-{session_id}")),
                x_grok_req_id: Some(format!("xai-lhc-{}-{}", label, uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id),
                x_grok_turn_idx: None,
                x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                x_grok_deployment_id: None,
                x_grok_user_id: None,
                trace: None,
                prompt_cache_key: None,
                reasoning_effort: None,
                json_schema: None,
            };
            let fut = client.conversation_collect(request);
            match tokio::time::timeout(timeout, fut).await {
                Ok(Ok(response)) => {
                    let text = response.assistant_text();
                    if text.trim().is_empty() {
                        return Err(LhcInferenceError {
                            kind: LhcInferenceErrorKind::Refusal,
                            detail: "empty assistant text".into(),
                            request_messages: Some(request_messages),
                        });
                    }
                    Ok(LhcInferenceSample {
                        text,
                        model,
                        prompt_label: label,
                        request_messages,
                    })
                }
                Ok(Err(err)) => {
                    let detail = err.to_string();
                    let kind = if detail.contains("401") || detail.contains("403") {
                        LhcInferenceErrorKind::Refusal
                    } else if detail.to_lowercase().contains("cancel") {
                        LhcInferenceErrorKind::Cancelled
                    } else {
                        LhcInferenceErrorKind::Transport
                    };
                    Err(LhcInferenceError {
                        kind,
                        detail,
                        request_messages: Some(request_messages),
                    })
                }
                Err(_) => Err(LhcInferenceError {
                    kind: LhcInferenceErrorKind::Timeout,
                    detail: format!("inference timed out after {timeout:?}"),
                    request_messages: Some(request_messages),
                }),
            }
        })
    }
}
