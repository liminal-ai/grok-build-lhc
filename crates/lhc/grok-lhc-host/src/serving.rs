//! Request-context serving: LHC view → host `ConversationItem`s (Chunk 2).
//!
//! Hook 4 lives in the shell (`turn.rs`) after `build_request`. This module
//! translates and validates the substitution so the shell can swap `items`
//! without touching system/tool host ownership.

use lhc::shared_tech::view::{LlmRequestContext, LlmRequestContextPartType, LlmRequestContextRole};
use tracing::warn;
use xai_grok_sampling_types::ConversationItem;

/// Outcome of attempting to build a substituted conversation body.
#[derive(Debug)]
pub enum ServeDecision {
    /// Use these items as the full conversation body (system prefix already merged).
    Substitute { items: Vec<ConversationItem> },
    /// Keep the native `build_request` body (fail-open).
    Native { reason: &'static str },
}

/// Split host-owned leading `System` items from the rest of a native request body.
pub fn split_system_prefix(
    native: &[ConversationItem],
) -> (Vec<ConversationItem>, Vec<ConversationItem>) {
    let mut prefix = Vec::new();
    let mut rest = Vec::new();
    let mut in_prefix = true;
    for item in native {
        match item {
            ConversationItem::System(_) if in_prefix => prefix.push(item.clone()),
            other => {
                in_prefix = false;
                rest.push(other.clone());
            }
        }
    }
    (prefix, rest)
}

/// Translate an LHC request context into host conversation items.
///
/// Only `User` / `Assistant` roles exist in the view — no tool calls — so the
/// result cannot contain a dangling tool call.
pub fn context_to_conversation_items(
    ctx: &LlmRequestContext,
) -> Result<Vec<ConversationItem>, String> {
    let mut out = Vec::with_capacity(ctx.messages.len());
    for msg in &ctx.messages {
        let text = parts_to_text(&msg.content)?;
        match msg.role {
            LlmRequestContextRole::User => out.push(ConversationItem::user(text)),
            LlmRequestContextRole::Assistant => out.push(ConversationItem::assistant(text)),
        }
    }
    Ok(out)
}

fn parts_to_text(
    parts: &[lhc::shared_tech::view::LlmRequestContextPart],
) -> Result<String, String> {
    let mut chunks = Vec::with_capacity(parts.len());
    for part in parts {
        match part.type_ {
            LlmRequestContextPartType::Text => chunks.push(part.text.clone()),
        }
    }
    Ok(chunks.join("\n"))
}

/// True when a body contains tool-call or tool-result items (must not appear
/// after LHC substitution).
pub fn body_has_tool_cycle(items: &[ConversationItem]) -> bool {
    items.iter().any(|item| match item {
        ConversationItem::ToolResult(_) | ConversationItem::BackendToolCall(_) => true,
        ConversationItem::Assistant(a) => !a.tool_calls.is_empty(),
        ConversationItem::System(_)
        | ConversationItem::User(_)
        | ConversationItem::Reasoning(_) => false,
    })
}

/// Build the all-LHC or all-native decision for one request.
///
/// - Preserves native leading `System` items (host-owned preamble).
/// - Rejects empty contexts and any translated body that somehow has tools.
/// - Does **not** alter `prompt_index` — that lives on chat-state / is stamped
///   onto the request by the shell after this substitution.
pub fn decide_substitution(
    native_items: &[ConversationItem],
    ctx: &LlmRequestContext,
) -> ServeDecision {
    let body = match context_to_conversation_items(ctx) {
        Ok(b) => b,
        Err(err) => {
            warn!(%err, "LHC serving: context translation failed; native path");
            return ServeDecision::Native {
                reason: "context_translation_failed",
            };
        }
    };
    if body.is_empty() {
        return ServeDecision::Native {
            reason: "empty_lhc_context",
        };
    }
    if body_has_tool_cycle(&body) {
        // Defensive — the LHC view has no tool representation.
        return ServeDecision::Native {
            reason: "lhc_body_has_tools",
        };
    }
    let (system_prefix, _) = split_system_prefix(native_items);
    let mut items = system_prefix;
    items.extend(body);
    ServeDecision::Substitute { items }
}

/// Apply substitution to a request's items, leaving tools/model/params intact.
///
/// Token accounting: `ConversationRequest` carries no body-token field; the
/// provider counts tokens from the substituted wire body. Host
/// `total_tokens` / compaction triggers remain on the native conversation
/// (actor state) and are intentionally not rewritten here — see MAPPING.md.
pub fn apply_serve_decision(
    native_items: Vec<ConversationItem>,
    decision: ServeDecision,
) -> (Vec<ConversationItem>, bool) {
    match decision {
        ServeDecision::Substitute { items } => (items, true),
        ServeDecision::Native { reason } => {
            warn!(reason, "LHC serving: using native conversation body");
            (native_items, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lhc::shared_tech::view::{LlmRequestContextMessage, LlmRequestContextPart};

    fn ctx(messages: Vec<(LlmRequestContextRole, &str)>) -> LlmRequestContext {
        LlmRequestContext {
            thread_id: "t".into(),
            messages: messages
                .into_iter()
                .map(|(role, text)| LlmRequestContextMessage {
                    role,
                    content: vec![LlmRequestContextPart {
                        type_: LlmRequestContextPartType::Text,
                        text: text.into(),
                    }],
                })
                .collect(),
        }
    }

    #[test]
    fn preserves_system_prefix_and_drops_native_body() {
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("old"),
            ConversationItem::assistant("old-a"),
        ];
        let c = ctx(vec![
            (LlmRequestContextRole::User, "new-u"),
            (LlmRequestContextRole::Assistant, "new-a"),
        ]);
        match decide_substitution(&native, &c) {
            ServeDecision::Substitute { items } => {
                assert_eq!(items.len(), 3);
                assert!(matches!(&items[0], ConversationItem::System(_)));
                assert!(matches!(&items[1], ConversationItem::User(_)));
                assert!(matches!(&items[2], ConversationItem::Assistant(_)));
            }
            ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
        }
    }

    #[test]
    fn substituted_body_has_no_tool_cycle() {
        let c = ctx(vec![(LlmRequestContextRole::User, "hi")]);
        let body = context_to_conversation_items(&c).unwrap();
        assert!(!body_has_tool_cycle(&body));
    }

    #[test]
    fn empty_context_fails_open() {
        let native = vec![ConversationItem::user("u")];
        let c = ctx(vec![]);
        assert!(matches!(
            decide_substitution(&native, &c),
            ServeDecision::Native {
                reason: "empty_lhc_context"
            }
        ));
    }

    /// Accounting item 1 — token totals live on the actor, not the request.
    /// Substitution rewrites `items` only; a parallel host token counter is
    /// untouched (mysterious truncation if we ever "sync" them).
    #[test]
    fn accounting_token_totals_unaffected_by_substitute() {
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("old-long-conversation"),
        ];
        let host_total_tokens: u64 = 12_345;
        let c = ctx(vec![(LlmRequestContextRole::User, "short")]);
        let decision = decide_substitution(&native, &c);
        let (items, substituted) = apply_serve_decision(native, decision);
        assert!(substituted);
        assert!(matches!(&items[1], ConversationItem::User(_)));
        assert_eq!(host_total_tokens, 12_345);
    }

    /// Accounting item 2 — image byte-budget eviction ran on the native body
    /// inside `build_request`. On Substitute that body is discarded; the wire
    /// body is the LHC view (text-only). Native eviction is therefore
    /// **unaffected / moot for the wire**.
    #[test]
    fn accounting_image_eviction_moot_on_substitute_wire() {
        use xai_grok_sampling_types::{ContentPart, UserItem};
        let mut user = ConversationItem::user("see");
        if let ConversationItem::User(UserItem { content, .. }) = &mut user {
            content.push(ContentPart::Image {
                url: "data:image/png;base64,AAAA".into(),
            });
        }
        let native = vec![ConversationItem::system("sys"), user];
        let c = ctx(vec![(LlmRequestContextRole::User, "see [image omitted]")]);
        match decide_substitution(&native, &c) {
            ServeDecision::Substitute { items } => {
                assert!(matches!(&items[0], ConversationItem::System(_)));
                // LHC body has no Image parts.
                match &items[1] {
                    ConversationItem::User(u) => {
                        assert!(
                            u.content
                                .iter()
                                .all(|p| !matches!(p, ContentPart::Image { .. }))
                        );
                    }
                    other => panic!("expected user, got {other:?}"),
                }
            }
            ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
        }
    }

    /// Accounting item 3 — memory reminder is host-owned system text; the
    /// leading native `System` prefix is preserved across substitution.
    #[test]
    fn accounting_memory_reminder_preserved_in_system_prefix() {
        let native = vec![
            ConversationItem::system("base\nRemember: user likes rust"),
            ConversationItem::user("old"),
        ];
        let c = ctx(vec![(LlmRequestContextRole::User, "new")]);
        match decide_substitution(&native, &c) {
            ServeDecision::Substitute { items } => match &items[0] {
                ConversationItem::System(s) => {
                    assert!(s.content.contains("Remember: user likes rust"));
                }
                other => panic!("expected system prefix, got {other:?}"),
            },
            ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
        }
    }

    /// Accounting item 4 — integrity repair ran on the actor before
    /// `build_request`. The LHC view has no tool cycle, so dangling-tool
    /// integrity is N/A on the substitute path (never introduced).
    #[test]
    fn accounting_integrity_no_tool_cycle_on_substitute() {
        use xai_grok_sampling_types::ToolCall;
        let assistant = ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }]);
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            assistant,
            // Mid-tool-cycle native body (no result yet).
        ];
        assert!(body_has_tool_cycle(&native[1..]));
        let c = ctx(vec![
            (LlmRequestContextRole::User, "run"),
            (LlmRequestContextRole::Assistant, "calling"),
        ]);
        match decide_substitution(&native, &c) {
            ServeDecision::Substitute { items } => {
                assert!(!body_has_tool_cycle(&items));
                assert!(matches!(&items[0], ConversationItem::System(_)));
            }
            ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
        }
    }

    /// `prompt_index` / `x_grok_turn_idx` are stamped by the shell from actor
    /// state after substitution — apply does not invent or bump them.
    #[test]
    fn prompt_index_not_owned_by_serve_decision() {
        let prompt_index: u64 = 7;
        let native = vec![ConversationItem::user("u")];
        let c = ctx(vec![(LlmRequestContextRole::User, "v")]);
        let (_, substituted) = apply_serve_decision(
            native,
            decide_substitution(&[ConversationItem::user("u")], &c),
        );
        assert!(substituted);
        assert_eq!(prompt_index, 7);
    }
}
