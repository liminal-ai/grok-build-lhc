//! Production-path final tool-result fidelity for LHC retrieval tools.
//!
//! `get_turns` / `get_messages` return SDK-formatted historical envelopes.
//! After `handle_bridge_tool_success`, the model-visible `ConversationItem`
//! must retain those bytes exactly — no base64/PDF extraction (no live image
//! follow-ups) and no worktree PathRewriter on historical cwd strings.

use super::support::*;
use super::*;
use xai_grok_sampling_types::{ContentPart, ConversationItem};
use xai_grok_tools::types::output::{TextOutput, ToolOutput, ToolRunResult};

fn tool_result_text(item: &ConversationItem) -> &str {
    match item {
        ConversationItem::ToolResult(tr) => tr.content.as_ref(),
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

fn followup_has_data_image(followups: &[ConversationItem]) -> bool {
    followups.iter().any(|item| match item {
        ConversationItem::User(u) => u
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Image { url } if url.starts_with("data:image/"))),
        _ => false,
    })
}

/// Historical SDK envelope with image + PDF data URIs and plain + URL-encoded
/// real cwd paths — all must survive finalization byte-for-byte.
fn historical_sdk_envelope(real_cwd: &str) -> String {
    let encoded = urlencoding::encode(real_cwd);
    format!(
        "<recall tool=\"get_messages\">\n\
         <m1>\n\
         screenshot was data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==\n\
         and a doc data:application/pdf;base64,JVBERi0xLjQKJeLjz9MKMyAwIG9iago8PC9UeXBlL1BhZ2UKPj4KZW5kb2JqCnhyZWYKMCAwCjAwMDAwMDAwMDAgNjU1MzUgZiAKdHJhaWxlcgo8PC9TaXplIDQ+Pj4Kc3RhcnR4cmVmCjEwJSVFT0YK\n\
         cwd plain: {real_cwd}/src/main.rs\n\
         cwd encoded: {encoded}%2Fsrc%2Fmain.rs\n\
         </m1>\n\
         </recall>"
    )
}

fn text_run_result(prompt: &str) -> ToolRunResult {
    ToolRunResult {
        output: ToolOutput::Text(TextOutput::from(prompt)),
        prompt_text: prompt.to_owned(),
        effective_tool_name: None,
    }
}

/// Production final path: `handle_bridge_tool_success` → model-visible tool_result.
#[tokio::test(flavor = "current_thread")]
async fn handle_bridge_tool_success_preserves_get_messages_sdk_bytes_exactly() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            assert!(!actor.is_cursor_harness());

            // Worktree rewrite would map real → display if not exempted.
            let real_cwd = actor.session_info.cwd.clone();
            let display_cwd = "/project/original".to_string();
            assert_ne!(
                real_cwd, display_cwd,
                "test needs real_cwd ≠ display for PathRewriter coverage"
            );
            actor
                .display_cwd
                .set(display_cwd.clone())
                .expect("display_cwd set once");
            assert!(
                actor.path_rewriter().is_some(),
                "path rewriter must be active so exemption is exercised"
            );

            let sdk_text = historical_sdk_envelope(&real_cwd);
            let parsed_args = serde_json::json!({ "ids": ["m1"] });
            let followups = actor
                .handle_bridge_tool_success(
                    &acp::ToolCallId::new("tc-lhc-msg"),
                    "tc-lhc-msg",
                    grok_lhc_host::GET_MESSAGES_TOOL_NAME,
                    grok_lhc_host::GET_MESSAGES_TOOL_NAME,
                    DrainedToolSuccess::new(text_run_result(&sdk_text)),
                    0,
                    "test-model",
                    &parsed_args,
                )
                .await
                .expect("bridge success");

            assert!(
                followups.is_empty(),
                "retrieval must not emit deferred image follow-ups: {followups:?}"
            );
            assert!(
                !followup_has_data_image(&followups),
                "no live image messages from historical data URIs"
            );

            let conv = actor.chat_state_handle.get_conversation().await;
            let tool = conv
                .iter()
                .rev()
                .find(|i| matches!(i, ConversationItem::ToolResult(_)))
                .expect("tool result pushed");
            let text = tool_result_text(tool);
            assert_eq!(
                text, sdk_text,
                "final tool-result text must match SDK envelope byte-for-byte"
            );
            // Explicit regressions the exemption exists to prevent:
            assert!(
                text.contains("data:image/png;base64,"),
                "historical image data URI must remain: {text}"
            );
            assert!(
                text.contains("data:application/pdf;base64,"),
                "historical PDF data URI must remain: {text}"
            );
            assert!(
                text.contains(&real_cwd),
                "plain real cwd must not be rewritten to display: {text}"
            );
            assert!(
                text.contains(urlencoding::encode(&real_cwd).as_ref()),
                "URL-encoded real cwd must not be rewritten: {text}"
            );
            assert!(
                !text.contains(&display_cwd),
                "display cwd must not appear after rewrite exemption: {text}"
            );
            assert!(
                !text.contains("[image content will be provided separately]"),
                "must not replace historical images with extraction placeholder: {text}"
            );
        })
        .await;
}

/// Same exemption for get_turns (symmetric production path).
#[tokio::test(flavor = "current_thread")]
async fn handle_bridge_tool_success_preserves_get_turns_sdk_bytes_exactly() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let real_cwd = actor.session_info.cwd.clone();
            actor
                .display_cwd
                .set("/other/display/path".into())
                .expect("display_cwd");
            let sdk_text = historical_sdk_envelope(&real_cwd);
            let followups = actor
                .handle_bridge_tool_success(
                    &acp::ToolCallId::new("tc-lhc-turns"),
                    "tc-lhc-turns",
                    grok_lhc_host::GET_TURNS_TOOL_NAME,
                    grok_lhc_host::GET_TURNS_TOOL_NAME,
                    DrainedToolSuccess::new(text_run_result(&sdk_text)),
                    0,
                    "test-model",
                    &serde_json::json!({ "ids": ["t1"] }),
                )
                .await
                .expect("bridge success");
            assert!(followups.is_empty(), "no live follow-ups: {followups:?}");
            let conv = actor.chat_state_handle.get_conversation().await;
            let tool = conv
                .iter()
                .rev()
                .find(|i| matches!(i, ConversationItem::ToolResult(_)))
                .expect("tool result");
            assert_eq!(tool_result_text(tool), sdk_text);
        })
        .await;
}

/// Unrelated tools still get extraction + path rewrite (exemption is narrow).
#[tokio::test(flavor = "current_thread")]
async fn handle_bridge_tool_success_still_rewrites_unrelated_tool_paths() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let real_cwd = actor.session_info.cwd.clone();
            let display = "/display/root".to_string();
            actor.display_cwd.set(display.clone()).expect("display_cwd");
            let body = format!("wrote file at {real_cwd}/out.txt");
            let _ = actor
                .handle_bridge_tool_success(
                    &acp::ToolCallId::new("tc-bash"),
                    "tc-bash",
                    "bash",
                    "bash",
                    DrainedToolSuccess::new(text_run_result(&body)),
                    0,
                    "test-model",
                    &serde_json::json!({}),
                )
                .await
                .expect("bridge success");
            let conv = actor.chat_state_handle.get_conversation().await;
            let tool = conv
                .iter()
                .rev()
                .find(|i| matches!(i, ConversationItem::ToolResult(_)))
                .expect("tool result");
            let text = tool_result_text(tool);
            assert!(
                text.contains(&display),
                "unrelated tools must still path-rewrite: {text}"
            );
            assert!(
                !text.contains(&real_cwd),
                "real cwd should be rewritten for non-retrieval tools: {text}"
            );
        })
        .await;
}
