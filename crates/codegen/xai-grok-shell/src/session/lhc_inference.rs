//! Shell-side [`grok_lhc_host::LhcInferenceSampler`] — dedicated LHC transport.
//!
//! Builds a fresh client per call (credentials re-resolved via
//! [`AuthManager`] / bearer resolver), using a dedicated non-main model when
//! `GROK_LHC_INFERENCE_MODEL` is set; otherwise the session model with a
//! distinct request id. Mirrors compaction's `prepare_chat_completion`
//! refresh pattern, not a spawn-time credential snapshot.

use std::sync::Arc;
use std::time::Duration;

use grok_lhc_host::{
    InferenceRequestMessage, InferenceRequestRole, LhcInferenceError, LhcInferenceErrorKind,
    LhcInferenceFuture, LhcInferenceRequest, LhcInferenceSample, LhcInferenceSampler,
};
use tokio_util::sync::CancellationToken;
use xai_grok_sampling_types::{ConversationItem, ConversationRequest};

use crate::auth::AuthManager;
use crate::sampling::Client as OaiCompatClient;
use crate::sampling::{SamplerConfig, SamplingError};

/// Env override for a dedicated non-main LHC inference model slug.
const LHC_INFERENCE_MODEL_ENV: &str = "GROK_LHC_INFERENCE_MODEL";

/// Inference sampler wired at LHC tee/open (hook 2).
pub struct ShellLhcInferenceSampler {
    /// Template config (URL, backend, headers). Model may be overridden per call.
    base_config: SamplerConfig,
    auth_manager: Option<Arc<AuthManager>>,
    session_id: String,
    timeout: Duration,
    /// Dedicated non-main model slug when set; else `base_config.model`.
    dedicated_model: Option<String>,
}

impl ShellLhcInferenceSampler {
    pub fn new(
        base_config: SamplerConfig,
        auth_manager: Option<Arc<AuthManager>>,
        session_id: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        let dedicated_model = std::env::var(LHC_INFERENCE_MODEL_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            base_config,
            auth_manager,
            session_id: session_id.into(),
            timeout,
            dedicated_model,
        }
    }

    pub fn into_arc(self) -> Arc<dyn LhcInferenceSampler> {
        Arc::new(self)
    }

    fn model_slug(&self) -> String {
        self.dedicated_model
            .clone()
            .unwrap_or_else(|| self.base_config.model.clone())
    }

    /// Rebuild a client with live credentials (compaction-style refresh).
    async fn resolve_client(&self) -> Result<(OaiCompatClient, String), LhcInferenceError> {
        let mut cfg = self.base_config.clone();
        let model = self.model_slug();
        cfg.model = model.clone();
        if let Some(am) = &self.auth_manager {
            // Refresh session token when expired / near-expiry.
            let _ = am.auth().await;
            if let Some(auth) = am.current_wire_valid() {
                cfg.api_key = Some(auth.key);
            }
            // Live resolve for subsequent 401 races.
            struct AmBearer(Arc<AuthManager>);
            impl std::fmt::Debug for AmBearer {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.debug_struct("AmBearer").finish()
                }
            }
            impl xai_grok_sampler::BearerResolver for AmBearer {
                fn current_bearer(&self) -> Option<String> {
                    self.0.current_wire_valid().map(|a| a.key)
                }
            }
            cfg.bearer_resolver = Some(std::sync::Arc::new(AmBearer(am.clone())));
        }
        OaiCompatClient::new(cfg)
            .map(|c| (c, model))
            .map_err(|e| LhcInferenceError {
                kind: LhcInferenceErrorKind::Unavailable,
                detail: e.to_string(),
                request_messages: None,
            })
    }
}

fn classify_sampling_error(err: &SamplingError) -> LhcInferenceErrorKind {
    match err {
        SamplingError::Auth(_) => LhcInferenceErrorKind::Auth,
        SamplingError::IdleTimeout { .. } => LhcInferenceErrorKind::Timeout,
        SamplingError::EmptyResponse { .. } | SamplingError::MaxTokensTruncation => {
            LhcInferenceErrorKind::Refusal
        }
        SamplingError::Http(_)
        | SamplingError::Api { .. }
        | SamplingError::InvalidConfiguration(_)
        | SamplingError::Serialization(_)
        | SamplingError::EventStreamError(_)
        | SamplingError::StreamError { .. }
        | SamplingError::DoomLoopDetected { .. } => LhcInferenceErrorKind::Transport,
    }
}

impl LhcInferenceSampler for ShellLhcInferenceSampler {
    fn sample(&self, req: LhcInferenceRequest, cancel: CancellationToken) -> LhcInferenceFuture {
        let timeout = self.timeout;
        let session_id = self.session_id.clone();
        let this = ShellLhcInferenceSampler {
            base_config: self.base_config.clone(),
            auth_manager: self.auth_manager.clone(),
            session_id: self.session_id.clone(),
            timeout: self.timeout,
            dedicated_model: self.dedicated_model.clone(),
        };
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(LhcInferenceError {
                    kind: LhcInferenceErrorKind::Cancelled,
                    detail: "cancelled before sample".into(),
                    request_messages: None,
                });
            }
            let (client, model) = this.resolve_client().await?;
            let label = req.op().as_prompt_label().to_string();
            let max_output_tokens = req.max_output_tokens();
            let user_text = req.user_prompt_text();
            let system = format!(
                "You are a Grok Build helper performing `{label}` for long-horizon context. \
                 Reply with only the transformed text, no preamble. \
                 Respect the target token budget implied by the user message."
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
                max_output_tokens: Some(max_output_tokens),
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
            tokio::select! {
                _ = cancel.cancelled() => Err(LhcInferenceError {
                    kind: LhcInferenceErrorKind::Cancelled,
                    detail: "cancelled during sample".into(),
                    request_messages: Some(request_messages),
                }),
                result = tokio::time::timeout(timeout, fut) => {
                    match result {
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
                                max_output_tokens,
                            })
                        }
                        Ok(Err(err)) => Err(LhcInferenceError {
                            kind: classify_sampling_error(&err),
                            detail: err.to_string(),
                            request_messages: Some(request_messages),
                        }),
                        Err(_) => Err(LhcInferenceError {
                            kind: LhcInferenceErrorKind::Timeout,
                            detail: format!("inference timed out after {timeout:?}"),
                            request_messages: Some(request_messages),
                        }),
                    }
                }
            }
        })
    }
}
