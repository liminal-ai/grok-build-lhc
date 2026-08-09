//! LHC Wave B retrieval tools — direct ToolBridge registration.
//!
//! Registered only while capture is active for the owning session. Tools bind
//! the session id at registration time and resolve the capture handle per call
//! so inactive/cross-session access fails explicitly.
//!
//! Args are raw JSON (`serde_json::Value`) with an explicit schema override so
//! host validation can refuse null `from`, unknown fields, and bad ids **before**
//! any SDK call (zero impressions). Typed serde would accept `from: null` as
//! `None`.

use std::sync::Arc;

use grok_lhc_host::{
    GET_MESSAGES_TOOL_NAME, GET_TURNS_TOOL_NAME, get_messages_description, get_turns_description,
    retrieval_args_schema, run_get_messages, run_get_turns,
};
use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::types::tool::{ToolKind, ToolNamespace};
use xai_grok_tools::types::tool_metadata::ToolMetadata;
use xai_tool_runtime::Tool;

/// Session binding shared by both tools.
#[derive(Debug, Clone)]
pub(crate) struct LhcRetrievalSession {
    pub session_id: Arc<str>,
}

#[derive(Debug, Clone)]
pub(crate) struct GetTurnsTool {
    pub session: LhcRetrievalSession,
}

#[derive(Debug, Clone)]
pub(crate) struct GetMessagesTool {
    pub session: LhcRetrievalSession,
}

fn turns_description() -> &'static str {
    static DESC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DESC.get_or_init(get_turns_description).as_str()
}

fn messages_description() -> &'static str {
    static DESC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DESC.get_or_init(get_messages_description).as_str()
}

impl ToolMetadata for GetTurnsTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        turns_description()
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

impl ToolMetadata for GetMessagesTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        messages_description()
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

impl Tool for GetTurnsTool {
    // Raw JSON so host can refuse null/unknown strictly (see module docs).
    type Args = serde_json::Value;
    type Output = String;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(GET_TURNS_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(GET_TURNS_TOOL_NAME, turns_description())
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        _ctx: xai_tool_runtime::ToolCallContext,
        input: serde_json::Value,
    ) -> Result<String, xai_tool_runtime::ToolError> {
        run_get_turns(&self.session.session_id, &input)
            .await
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)
    }
}

impl Tool for GetMessagesTool {
    type Args = serde_json::Value;
    type Output = String;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(GET_MESSAGES_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(GET_MESSAGES_TOOL_NAME, messages_description())
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        _ctx: xai_tool_runtime::ToolCallContext,
        input: serde_json::Value,
    ) -> Result<String, xai_tool_runtime::ToolError> {
        run_get_messages(&self.session.session_id, &input)
            .await
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)
    }
}

/// Register both retrieval tools for an LHC-active session.
///
/// Idempotent: removes prior registration first (rebuild / re-on safe).
pub(crate) async fn register_lhc_retrieval_tools(
    bridge: &ToolBridge,
    session_id: &str,
) -> Result<(), String> {
    unregister_lhc_retrieval_tools(bridge);

    let session = LhcRetrievalSession {
        session_id: Arc::from(session_id),
    };
    let turns_schema = retrieval_args_schema("Turn ids, e.g. t211");
    let messages_schema = retrieval_args_schema("Message ids, e.g. m3177");

    bridge
        .register_mcp_tools(
            GET_TURNS_TOOL_NAME.to_owned(),
            GetTurnsTool {
                session: session.clone(),
            },
            Some(turns_schema),
        )
        .await
        .map_err(|e| format!("failed to register get_turns: {e}"))?;
    bridge
        .register_mcp_tools(
            GET_MESSAGES_TOOL_NAME.to_owned(),
            GetMessagesTool { session },
            Some(messages_schema),
        )
        .await
        .map_err(|e| format!("failed to register get_messages: {e}"))?;
    Ok(())
}

/// Remove retrieval tools from the bridge (capture off / teardown).
pub(crate) fn unregister_lhc_retrieval_tools(bridge: &ToolBridge) {
    let _ = bridge.unregister_tool_by_name(GET_TURNS_TOOL_NAME);
    let _ = bridge.unregister_tool_by_name(GET_MESSAGES_TOOL_NAME);
}

/// Whether either retrieval tool is currently registered.
pub(crate) async fn lhc_retrieval_tools_registered(bridge: &ToolBridge) -> bool {
    let names = bridge.tool_definitions().await;
    names.iter().any(|d| {
        d.function.name == GET_TURNS_TOOL_NAME || d.function.name == GET_MESSAGES_TOOL_NAME
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use grok_lhc_host::{
        GET_MESSAGES_TOOL_NAME, GET_TURNS_TOOL_NAME, capture_active, env_lock, spawn_capture,
    };
    use xai_grok_sampling_types::ConversationItem;
    use xai_grok_tools::bridge::ToolBridge;

    #[tokio::test]
    async fn toolbridge_register_dispatch_and_lifecycle() {
        let _g = env_lock();
        let root = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("GROK_LHC");
        let prev_root = std::env::var_os("GROK_LHC_ROOT");
        unsafe {
            std::env::set_var("GROK_LHC", "1");
            std::env::set_var("GROK_LHC_ROOT", root.path());
        }
        let sid = "shell-tb-retrieval";
        let handle = spawn_capture(sid, Some("/tmp"), &[], Some(root.path()), None).unwrap();
        handle.persist(&ConversationItem::user("what does the config do?"));
        handle.persist(&ConversationItem::assistant("it configures the server"));
        handle.flush().await.expect("flush");
        // Wait for turn close.
        for _ in 0..80 {
            if let Ok(ev) = handle.list_events().await
                && ev.iter().any(|e| e.event_kind().as_str() == "turn_end")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        let bridge = ToolBridge::for_test();
        assert!(!lhc_retrieval_tools_registered(&bridge).await);
        register_lhc_retrieval_tools(&bridge, sid)
            .await
            .expect("register");
        assert!(lhc_retrieval_tools_registered(&bridge).await);

        let defs = bridge.tool_definitions().await;
        let turns_def = defs
            .iter()
            .find(|d| d.function.name == GET_TURNS_TOOL_NAME)
            .expect("get_turns def");
        let desc = turns_def.function.description.as_deref().unwrap_or("");
        assert!(
            desc.contains("<tNNN>") || desc.contains("turn id"),
            "description must teach labels: {desc}"
        );
        // Schema must advertise ids + from only.
        let params = &turns_def.function.parameters;
        assert_eq!(params["additionalProperties"], false);
        assert!(params["properties"]["ids"].is_object());
        assert!(params["properties"]["from"].is_object());

        // Real ToolBridge dispatch.
        let result = bridge
            .call(
                GET_TURNS_TOOL_NAME,
                serde_json::json!({ "ids": ["t1"] }),
                "call-tb-1",
            )
            .await
            .expect("bridge call get_turns");
        assert!(
            result.prompt_text.contains("what does the config do?")
                || result.prompt_text.contains("<t1>")
                || result.prompt_text.contains("not served"),
            "unexpected prompt_text: {}",
            result.prompt_text
        );

        // Invalid args via bridge: error, no panic.
        let err = bridge
            .call(
                GET_TURNS_TOOL_NAME,
                serde_json::json!({ "ids": ["m1"] }),
                "call-tb-bad",
            )
            .await;
        assert!(err.is_err(), "message id on get_turns must refuse");

        // Unregister tears down tools.
        unregister_lhc_retrieval_tools(&bridge);
        assert!(!lhc_retrieval_tools_registered(&bridge).await);
        let missing = bridge
            .call(
                GET_TURNS_TOOL_NAME,
                serde_json::json!({ "ids": ["t1"] }),
                "call-tb-gone",
            )
            .await;
        assert!(missing.is_err(), "unregistered tool must not dispatch");

        // Messages tool registration + dispatch when re-registered.
        register_lhc_retrieval_tools(&bridge, sid)
            .await
            .expect("re-register");
        let _ = bridge
            .call(
                GET_MESSAGES_TOOL_NAME,
                serde_json::json!({ "ids": ["m1"] }),
                "call-tb-msg",
            )
            .await;

        // Inactive session after shutdown: tool still registered but resolve fails.
        // shutdown_blocking is !async — hop off the runtime.
        let handle_for_shutdown = handle.clone();
        tokio::task::spawn_blocking(move || handle_for_shutdown.shutdown_blocking())
            .await
            .expect("join shutdown");
        for _ in 0..50 {
            if !capture_active(sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let inactive = bridge
            .call(
                GET_TURNS_TOOL_NAME,
                serde_json::json!({ "ids": ["t1"] }),
                "call-tb-off",
            )
            .await;
        assert!(
            inactive.is_err(),
            "inactive capture must fail the tool call"
        );
        unregister_lhc_retrieval_tools(&bridge);

        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
        match prev_root {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
        }
    }
}
