//! Exhaustive `ConversationItem` → `MessageEventInput` mapping.
//!
//! See `MAPPING.md` for the decision table.

use lhc::intake_stream::MessageEventInput;
use serde_json::{Map, Value, json};
use xai_grok_sampling_types::{
    BackendToolCallItem, BackendToolKind, ContentPart, ConversationItem, SyntheticReason, ToolCall,
    ToolResultItem, reasoning_item_text, rs,
};

use crate::idempotency::{
    OccurrenceTracker, item_digest, item_event_key, model_change_key, thinking_level_change_key,
};

pub const ACTOR: &str = "grok";
pub const HARNESS: &str = "grok-build";

/// Max chars of an image URL retained in LHC text payloads (F14).
const IMAGE_URL_PREVIEW_CHARS: usize = 128;
/// Max chars of serialized backend tool outputs in a tool_result (A9).
const BACKEND_OUTPUTS_PREVIEW_CHARS: usize = 2048;

/// One mapped LHC event ready for `message_events`.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedEvent {
    pub input: MessageEventInput,
}

/// Map a single conversation item into zero or more LHC events.
pub fn map_item(
    session_id: &str,
    generation: u64,
    item: &ConversationItem,
    tracker: &mut OccurrenceTracker,
) -> Vec<MappedEvent> {
    let digest = item_digest(item);
    let occ = tracker.next(&digest);
    match item {
        ConversationItem::System(sys) => {
            vec![text_event(
                session_id,
                generation,
                &digest,
                occ,
                "runtime_note",
                sys.content.as_ref(),
                None,
            )]
        }
        ConversationItem::User(user) => match &user.synthetic_reason {
            None => {
                let text = content_parts_text(&user.content);
                vec![text_event(
                    session_id,
                    generation,
                    &digest,
                    occ,
                    "user_prompt",
                    &text,
                    None,
                )]
            }
            Some(reason) => map_synthetic_user(session_id, generation, &digest, occ, reason, user),
        },
        ConversationItem::Assistant(assistant) => {
            let mut out = Vec::new();
            let text = assistant.content.as_ref();
            if !text.is_empty() || assistant.tool_calls.is_empty() {
                out.push(text_event(
                    session_id,
                    generation,
                    &digest,
                    occ,
                    "assistant_text",
                    text,
                    None,
                ));
            }
            for tc in &assistant.tool_calls {
                out.push(tool_call_event(session_id, generation, &digest, occ, tc));
            }
            if assistant.tool_calls.is_empty() {
                out.push(turn_end_event(session_id, generation, &digest, occ, None));
            }
            out
        }
        ConversationItem::ToolResult(result) => {
            vec![tool_result_event(
                session_id, generation, &digest, occ, result,
            )]
        }
        ConversationItem::BackendToolCall(btc) => {
            backend_tool_events(session_id, generation, &digest, occ, btc)
        }
        ConversationItem::Reasoning(reasoning) => {
            let text = reasoning_item_text(reasoning);
            let part = if reasoning.id.is_empty() {
                None
            } else {
                Some(reasoning.id.as_str())
            };
            vec![text_event(
                session_id,
                generation,
                &digest,
                occ,
                "assistant_thinking",
                &text,
                part,
            )]
        }
    }
}

/// Exhaustive SyntheticReason mapping driven by `starts_prompt_turn()` (F12).
fn map_synthetic_user(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    reason: &SyntheticReason,
    user: &xai_grok_sampling_types::UserItem,
) -> Vec<MappedEvent> {
    let text = content_parts_text(&user.content);
    // Exhaustive on SyntheticReason — no `_ =>`.
    let starts_turn = match reason {
        SyntheticReason::TaskCompleted
        | SyntheticReason::SubagentCompleted
        | SyntheticReason::NotificationDrain
        | SyntheticReason::GoalClassifierNudge
        | SyntheticReason::SchedulerFired => {
            debug_assert!(reason.starts_prompt_turn());
            true
        }
        SyntheticReason::CompactionMeta
        | SyntheticReason::SystemReminder
        | SyntheticReason::ProjectInstructions
        | SyntheticReason::AutoContinue
        | SyntheticReason::AutoRecovery
        | SyntheticReason::Interjection
        | SyntheticReason::GoalSummary
        | SyntheticReason::StopHookFeedback
        | SyntheticReason::WorkingDirectorySwitch
        | SyntheticReason::Unknown => {
            debug_assert!(!reason.starts_prompt_turn());
            false
        }
    };
    let note = text_event(
        session_id,
        generation,
        digest,
        occ,
        "runtime_note",
        &text,
        None,
    );
    if starts_turn {
        // Close prior (possibly aborted) turn, then record the synthetic wake
        // as a runtime_note — never user_prompt (not real user input).
        vec![
            turn_end_event(session_id, generation, digest, occ, Some("pre_synthetic")),
            note,
        ]
    } else {
        vec![note]
    }
}

/// Map a full history slice; returns events and the aligned occurrence tracker.
pub fn map_history(
    session_id: &str,
    generation: u64,
    items: &[ConversationItem],
) -> (Vec<MappedEvent>, OccurrenceTracker) {
    let mut tracker = OccurrenceTracker::new();
    let mut out = Vec::new();
    for item in items {
        out.extend(map_item(session_id, generation, item, &mut tracker));
    }
    (out, tracker)
}

fn content_parts_text(parts: &[ContentPart]) -> String {
    let mut chunks = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ContentPart::Text { text } => chunks.push(text.as_ref().to_string()),
            ContentPart::Image { url } => {
                let preview = truncate_chars(url.as_ref(), IMAGE_URL_PREVIEW_CHARS);
                let suffix = if url.len() > IMAGE_URL_PREVIEW_CHARS {
                    format!("…({}B)", url.len())
                } else {
                    String::new()
                };
                chunks.push(format!("[image:{preview}{suffix}]"));
            }
        }
    }
    chunks.join("\n")
}

fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn text_event(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    kind: &str,
    text: &str,
    part: Option<&str>,
) -> MappedEvent {
    let key = item_event_key(session_id, generation, digest, occ, kind, part);
    MappedEvent {
        input: MessageEventInput {
            event_kind: kind.to_string(),
            idempotency_key: Some(key),
            actor: ACTOR.to_string(),
            harness: HARNESS.to_string(),
            payload: text_payload(text),
            extra: Map::new(),
        },
    }
}

fn turn_end_event(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    part: Option<&str>,
) -> MappedEvent {
    let key = item_event_key(session_id, generation, digest, occ, "turn_end", part);
    MappedEvent {
        input: MessageEventInput {
            event_kind: "turn_end".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR.to_string(),
            harness: HARNESS.to_string(),
            payload: Map::new(),
            extra: Map::new(),
        },
    }
}

fn tool_call_event(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    tc: &ToolCall,
) -> MappedEvent {
    let key = item_event_key(
        session_id,
        generation,
        digest,
        occ,
        "tool_call",
        Some(tc.id.as_ref()),
    );
    let arguments = parse_arguments_object(tc.arguments.as_ref());
    MappedEvent {
        input: MessageEventInput {
            event_kind: "tool_call".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR.to_string(),
            harness: HARNESS.to_string(),
            payload: tool_call_payload(tc.id.as_ref(), &tc.name, arguments),
            extra: Map::new(),
        },
    }
}

fn tool_result_event(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    result: &ToolResultItem,
) -> MappedEvent {
    let key = item_event_key(
        session_id,
        generation,
        digest,
        occ,
        "tool_result",
        Some(result.tool_call_id.as_str()),
    );
    let mut content = result.content.as_ref().to_string();
    if !result.images.is_empty() {
        let images = content_parts_text(&result.images);
        if content.is_empty() {
            content = images;
        } else {
            content = format!("{content}\n{images}");
        }
    }
    // Host ToolResultItem has no is_error field — omit rather than invent.
    let mut payload = Map::new();
    payload.insert("toolCallId".into(), json!(result.tool_call_id));
    payload.insert("content".into(), json!(content));
    MappedEvent {
        input: MessageEventInput {
            event_kind: "tool_result".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR.to_string(),
            harness: HARNESS.to_string(),
            payload,
            extra: Map::new(),
        },
    }
}

fn backend_tool_events(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    btc: &BackendToolCallItem,
) -> Vec<MappedEvent> {
    let id = btc.id().to_string();
    let (tool_name, arguments, result_content, is_error) = match &btc.kind {
        BackendToolKind::WebSearch(ws) => {
            let args = serde_json::to_value(&ws.action)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            let result = json!({
                "status": format!("{:?}", ws.status),
                "action": ws.action,
            })
            .to_string();
            ("web_search".to_string(), args, result, None)
        }
        BackendToolKind::XSearch(ct) => {
            let mut args = Map::new();
            args.insert("input".into(), json!(ct.input));
            let name = if ct.name.is_empty() {
                "x_search".to_string()
            } else {
                ct.name.clone()
            };
            // CustomToolCall has no embedded outputs; record the server-side call.
            let result = json!({
                "status": "server_executed",
                "callId": ct.call_id,
                "input": ct.input,
            })
            .to_string();
            (name, args, result, None)
        }
        BackendToolKind::CodeInterpreter(ci) => {
            let mut args = Map::new();
            args.insert("code".into(), json!(ci.code.clone().unwrap_or_default()));
            args.insert("containerId".into(), json!(ci.container_id));
            let is_error = matches!(ci.status, rs::CodeInterpreterToolCallStatus::Failed);
            let outputs_raw = serde_json::to_string(&ci.outputs).unwrap_or_else(|_| "null".into());
            let outputs_preview = truncate_chars(&outputs_raw, BACKEND_OUTPUTS_PREVIEW_CHARS);
            let outputs_value = if outputs_raw.len() > BACKEND_OUTPUTS_PREVIEW_CHARS {
                json!(format!("{outputs_preview}…({}B)", outputs_raw.len()))
            } else {
                ci.outputs
                    .as_ref()
                    .and_then(|o| serde_json::to_value(o).ok())
                    .unwrap_or(Value::Null)
            };
            let result = json!({
                "status": format!("{:?}", ci.status),
                "outputs": outputs_value,
            })
            .to_string();
            ("code_interpreter".to_string(), args, result, Some(is_error))
        }
    };

    let call_key = item_event_key(session_id, generation, digest, occ, "tool_call", Some(&id));
    let result_key = item_event_key(
        session_id,
        generation,
        digest,
        occ,
        "tool_result",
        Some(&id),
    );

    let mut result_payload = Map::new();
    result_payload.insert("toolCallId".into(), json!(id));
    result_payload.insert("content".into(), json!(result_content));
    if let Some(err) = is_error {
        result_payload.insert("isError".into(), json!(err));
    }

    vec![
        MappedEvent {
            input: MessageEventInput {
                event_kind: "tool_call".to_string(),
                idempotency_key: Some(call_key),
                actor: ACTOR.to_string(),
                harness: HARNESS.to_string(),
                payload: tool_call_payload(&id, &tool_name, arguments),
                extra: Map::new(),
            },
        },
        MappedEvent {
            input: MessageEventInput {
                event_kind: "tool_result".to_string(),
                idempotency_key: Some(result_key),
                actor: ACTOR.to_string(),
                harness: HARNESS.to_string(),
                payload: result_payload,
                extra: Map::new(),
            },
        },
    ]
}

fn text_payload(text: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("text".into(), json!(text));
    map
}

fn tool_call_payload(id: &str, name: &str, arguments: Map<String, Value>) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("toolCallId".into(), json!(id));
    map.insert("toolName".into(), json!(name));
    map.insert("arguments".into(), Value::Object(arguments));
    map
}

fn parse_arguments_object(raw: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => map,
        Ok(other) => {
            let mut map = Map::new();
            map.insert("value".into(), other);
            map
        }
        Err(_) => {
            let mut map = Map::new();
            map.insert("raw".into(), json!(raw));
            map
        }
    }
}

/// Build model_change / thinking_level_change events with ordinals.
///
/// Returns events and the next ordinal to use after these events.
pub fn map_model_change(
    session_id: &str,
    previous_model: &str,
    new_model: &str,
    previous_level: &str,
    new_level: &str,
    mut next_ordinal: u64,
) -> (Vec<MappedEvent>, u64) {
    let mut out = Vec::new();
    if previous_model != new_model {
        let key = model_change_key(session_id, next_ordinal, previous_model, new_model);
        next_ordinal = next_ordinal.saturating_add(1);
        let mut payload = Map::new();
        payload.insert("previousModel".into(), json!(previous_model));
        payload.insert("newModel".into(), json!(new_model));
        out.push(MappedEvent {
            input: MessageEventInput {
                event_kind: "model_change".to_string(),
                idempotency_key: Some(key),
                actor: ACTOR.to_string(),
                harness: HARNESS.to_string(),
                payload,
                extra: Map::new(),
            },
        });
    }
    if previous_level != new_level {
        let key = thinking_level_change_key(session_id, next_ordinal, previous_level, new_level);
        next_ordinal = next_ordinal.saturating_add(1);
        let mut payload = Map::new();
        payload.insert("previousLevel".into(), json!(previous_level));
        payload.insert("newLevel".into(), json!(new_level));
        out.push(MappedEvent {
            input: MessageEventInput {
                event_kind: "thinking_level_change".to_string(),
                idempotency_key: Some(key),
                actor: ACTOR.to_string(),
                harness: HARNESS.to_string(),
                payload,
                extra: Map::new(),
            },
        });
    }
    (out, next_ordinal)
}

/// Normalize optional reasoning effort to a nonempty LHC level string.
pub fn level_label(effort: Option<&str>) -> String {
    match effort {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "none".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::{ConversationItem, SyntheticReason, synthesized_reasoning_item};

    #[test]
    fn real_user_is_user_prompt() {
        let mut t = OccurrenceTracker::new();
        let ev = map_item("s", 0, &ConversationItem::user("hi"), &mut t);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].input.event_kind, "user_prompt");
    }

    #[test]
    fn turn_starting_synthetics_emit_turn_end_then_note() {
        let starters = [
            SyntheticReason::TaskCompleted,
            SyntheticReason::SubagentCompleted,
            SyntheticReason::NotificationDrain,
            SyntheticReason::GoalClassifierNudge,
            SyntheticReason::SchedulerFired,
        ];
        for reason in starters {
            let mut item = ConversationItem::user("wake");
            if let ConversationItem::User(u) = &mut item {
                u.synthetic_reason = Some(reason.clone());
            }
            let mut t = OccurrenceTracker::new();
            let ev = map_item("s", 0, &item, &mut t);
            assert_eq!(ev.len(), 2, "{reason:?}");
            assert_eq!(ev[0].input.event_kind, "turn_end");
            assert_eq!(ev[1].input.event_kind, "runtime_note");
        }
    }

    #[test]
    fn interjection_and_goal_summary_stay_plain_notes() {
        for reason in [SyntheticReason::Interjection, SyntheticReason::GoalSummary] {
            let mut item = ConversationItem::user("mid");
            if let ConversationItem::User(u) = &mut item {
                u.synthetic_reason = Some(reason);
            }
            let mut t = OccurrenceTracker::new();
            let ev = map_item("s", 0, &item, &mut t);
            assert_eq!(ev.len(), 1);
            assert_eq!(ev[0].input.event_kind, "runtime_note");
        }
    }

    #[test]
    fn assistant_without_tools_emits_turn_end() {
        let mut t = OccurrenceTracker::new();
        let ev = map_item("s", 0, &ConversationItem::assistant("done"), &mut t);
        assert!(ev.iter().any(|e| e.input.event_kind == "turn_end"));
    }

    #[test]
    fn assistant_with_tools_does_not_emit_turn_end() {
        let mut t = OccurrenceTracker::new();
        let item = ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: "x".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        });
        let ev = map_item("s", 0, &item, &mut t);
        assert!(!ev.iter().any(|e| e.input.event_kind == "turn_end"));
    }

    #[test]
    fn reasoning_is_assistant_thinking() {
        let mut t = OccurrenceTracker::new();
        let item = ConversationItem::Reasoning(synthesized_reasoning_item("think"));
        let ev = map_item("s", 0, &item, &mut t);
        assert_eq!(ev[0].input.event_kind, "assistant_thinking");
    }

    #[test]
    fn image_url_is_truncated() {
        let long = "data:image/png;base64,".to_string() + &"A".repeat(10_000);
        let text = content_parts_text(&[ContentPart::Image {
            url: long.clone().into(),
        }]);
        assert!(text.len() < long.len());
        assert!(text.contains("…("));
    }

    #[test]
    fn map_history_returns_aligned_tracker() {
        let (ev, mut t) = map_history("s", 0, &[ConversationItem::user("a")]);
        assert_eq!(ev.len(), 1);
        // Next identical item gets occ=1
        let d = item_digest(&ConversationItem::user("a"));
        assert_eq!(t.next(&d), 1);
    }
}
