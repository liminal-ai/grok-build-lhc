//! Shell-side [`grok_lhc_host::LhcInferenceSampler`] — dedicated LHC transport.
//!
//! Builds a fresh client per call (credentials re-resolved via
//! [`AuthManager`] / bearer resolver). Derivation inference uses
//! [`grok_lhc_host::DEFAULT_LHC_INFERENCE_MODEL`] (`grok-4.5`) by default,
//! with `GROK_LHC_INFERENCE_MODEL` as an explicit override — **never** the
//! session chat model. Thinking is fixed at
//! [`xai_grok_sampling_types::ReasoningEffort::Low`] (ruling).
//!
//! **Production drain lane (DERIV-12):** only `PromptSmoothing` reaches this
//! sampler from Background work. `ToolResultSummary` stays on the vendored
//! truncate-fallback (`FORCE_TOOL_RESULT_SUMMARY_FALLBACK`) and does **not**
//! call the sampler in production. The host callback bridge can still
//! dispatch a direct `SummarizeToolResult` (capability / probe); that is not
//! a production lane. Mirrors compaction's `prepare_chat_completion` refresh
//! pattern, not a spawn-time credential snapshot.

use std::sync::Arc;
use std::time::Duration;

use grok_lhc_host::{
    DEFAULT_LHC_INFERENCE_MODEL, InferenceRequestMessage, InferenceRequestRole, LhcInferenceError,
    LhcInferenceErrorKind, LhcInferenceFuture, LhcInferenceRequest, LhcInferenceSample,
    LhcInferenceSampler, resolved_inference_model,
};
use tokio_util::sync::CancellationToken;
use xai_grok_sampling_types::{ConversationItem, ConversationRequest, ReasoningEffort};

use crate::auth::AuthManager;
use crate::sampling::Client as OaiCompatClient;
use crate::sampling::{SamplerConfig, SamplingError};

/// Env override for a dedicated LHC derivation model slug.
const LHC_INFERENCE_MODEL_ENV: &str = "GROK_LHC_INFERENCE_MODEL";

/// Inference sampler wired at LHC tee/open (hook 2).
pub(crate) struct ShellLhcInferenceSampler {
    /// Template config (URL, backend, headers). Model is **not** taken from
    /// `base_config.model` for derivation calls — see [`Self::model_slug`].
    base_config: SamplerConfig,
    auth_manager: Option<Arc<AuthManager>>,
    session_id: String,
    timeout: Duration,
    /// Explicit override from `GROK_LHC_INFERENCE_MODEL` when set at construct.
    dedicated_model: Option<String>,
}

impl ShellLhcInferenceSampler {
    pub(crate) fn new(
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

    pub(crate) fn into_arc(self) -> Arc<dyn LhcInferenceSampler> {
        Arc::new(self)
    }

    /// Resolved derivation model: env/config override, else `grok-4.5`.
    ///
    /// Never falls back to the session's `base_config.model`.
    pub(crate) fn model_slug(&self) -> String {
        if let Some(ref m) = self.dedicated_model {
            return m.clone();
        }
        let (slug, _) = resolved_inference_model();
        // resolved_inference_model already defaults to grok-4.5; keep the
        // constant visible for readers of this call site.
        if slug.is_empty() {
            DEFAULT_LHC_INFERENCE_MODEL.to_string()
        } else {
            slug
        }
    }

    /// Fixed thinking level for derivation calls (ruling).
    pub(crate) fn thinking_level(&self) -> ReasoningEffort {
        ReasoningEffort::Low
    }

    /// Build the [`ConversationRequest`] that will be sent for a derivation.
    ///
    /// Kept as a pure constructor so tests can assert `reasoning_effort` (and
    /// model) on the request object itself — not only on accessors.
    pub(crate) fn build_derivation_request(
        &self,
        req: &LhcInferenceRequest,
        model: &str,
    ) -> ConversationRequest {
        let label = req.op().as_prompt_label();
        let max_output_tokens = req.max_output_tokens();
        let user_text = req.user_prompt_text();
        let system = format!(
            "You are a Grok Build helper performing `{label}` for long-horizon context. \
             Reply with only the transformed text, no preamble. \
             Respect the target token budget implied by the user message."
        );
        ConversationRequest {
            items: vec![
                ConversationItem::system(system),
                ConversationItem::user(user_text),
            ],
            tools: vec![],
            hosted_tools: vec![],
            tool_choice: None,
            model: Some(model.to_string()),
            temperature: Some(0.0),
            max_output_tokens: Some(max_output_tokens),
            top_p: None,
            x_grok_conv_id: Some(format!("lhc-inf-{}", self.session_id)),
            x_grok_req_id: Some(format!("xai-lhc-{}-{}", label, uuid::Uuid::new_v4())),
            x_grok_session_id: Some(self.session_id.clone()),
            x_grok_turn_idx: None,
            x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace: None,
            prompt_cache_key: None,
            reasoning_effort: Some(self.thinking_level()),
            json_schema: None,
        }
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
        SamplingError::Auth { .. } => LhcInferenceErrorKind::Auth,
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
            let request = this.build_derivation_request(&req, &model);
            let request_messages: Vec<InferenceRequestMessage> = request
                .items
                .iter()
                .filter_map(|item| {
                    let content = item.text_content();
                    let role = match item {
                        ConversationItem::System(_) => InferenceRequestRole::System,
                        ConversationItem::User(_) => InferenceRequestRole::User,
                        _ => return None,
                    };
                    Some(InferenceRequestMessage { role, content })
                })
                .collect();
            let _ = session_id; // stamped inside build_derivation_request
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn base_cfg(session_model: &str) -> SamplerConfig {
        SamplerConfig {
            base_url: "https://api.example.com".into(),
            model: session_model.into(),
            context_window: 128_000,
            ..Default::default()
        }
    }

    #[test]
    fn model_slug_defaults_to_grok_45_not_session_model() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC_INFERENCE_MODEL");
        unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") };
        let s = ShellLhcInferenceSampler::new(
            base_cfg("session-chat-model"),
            None,
            "test-sid",
            Duration::from_secs(1),
        );
        assert_eq!(s.model_slug(), "grok-4.5");
        assert_ne!(s.model_slug(), "session-chat-model");
        assert_eq!(s.thinking_level(), ReasoningEffort::Low);
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") },
        }
    }

    #[test]
    fn model_slug_honors_env_override() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC_INFERENCE_MODEL");
        unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", "override-model") };
        let s = ShellLhcInferenceSampler::new(
            base_cfg("session-chat-model"),
            None,
            "test-sid",
            Duration::from_secs(1),
        );
        assert_eq!(s.model_slug(), "override-model");
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") },
        }
    }

    /// M4 — pin `ReasoningEffort::Low` on the constructed request, not the accessor.
    #[test]
    fn derivation_request_carries_reasoning_effort_low() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC_INFERENCE_MODEL");
        unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") };
        let s = ShellLhcInferenceSampler::new(
            base_cfg("session-chat-model"),
            None,
            "m4-sid",
            Duration::from_secs(1),
        );
        let req = LhcInferenceRequest::SmoothPrompt {
            text: "rewrite me".into(),
            max_output_tokens: 32,
        };
        let built = s.build_derivation_request(&req, &s.model_slug());
        assert_eq!(
            built.reasoning_effort,
            Some(ReasoningEffort::Low),
            "derivation ConversationRequest must carry ReasoningEffort::Low"
        );
        assert_eq!(built.model.as_deref(), Some("grok-4.5"));
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") },
        }
    }

    /// Pin: derivation must NOT ship the session agent prompt / tools.
    ///
    /// Codex live cert found their derivation forwarded a ~20k-char agent
    /// prompt per call (4.7× cost undercount). This fork builds a purpose-built
    /// ~250-char system prompt with empty tools — assert so a future change
    /// cannot silently reintroduce session baggage.
    #[test]
    fn derivation_request_excludes_session_tools_and_agent_prompt() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC_INFERENCE_MODEL");
        unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") };
        let s = ShellLhcInferenceSampler::new(
            base_cfg("session-chat-model"),
            None,
            "pin-no-agent-prompt",
            Duration::from_secs(1),
        );
        let req = LhcInferenceRequest::SmoothPrompt {
            text: "rewrite me".into(),
            max_output_tokens: 32,
        };
        let built = s.build_derivation_request(&req, &s.model_slug());
        assert!(
            built.tools.is_empty(),
            "derivation must not carry session tools; got {} tools",
            built.tools.len()
        );
        assert!(
            built.hosted_tools.is_empty(),
            "derivation must not carry hosted tools; got {} hosted_tools",
            built.hosted_tools.len()
        );
        assert!(
            built.prompt_cache_key.is_none(),
            "derivation must set prompt_cache_key: None (write-back invalidates prefix cache)"
        );
        let system = built
            .items
            .iter()
            .find_map(|i| match i {
                ConversationItem::System(_) => Some(i.text_content()),
                _ => None,
            })
            .expect("derivation request must include a system item");
        assert!(
            system.len() < 500,
            "derivation system prompt must be purpose-built (~250 chars), not the \
             session agent prompt; got {} chars",
            system.len()
        );
        assert!(
            system.contains("lhc.smooth_prompt"),
            "system prompt should name the derivation op, not agent instructions"
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_INFERENCE_MODEL", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_INFERENCE_MODEL") },
        }
    }
}
