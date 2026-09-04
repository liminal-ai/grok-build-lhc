//! Exhaustive `ConversationItem` → `MessageEventInput` mapping.
//!
//! See `MAPPING.md` for the decision table.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lhc::intake_stream::MessageEventInput;
use serde_json::{Map, Value, json};
use xai_grok_sampling_types::{
    BackendToolCallItem, BackendToolKind, ContentPart, ConversationItem, SyntheticReason,
    TokenUsage, ToolCall, ToolResultItem, reasoning_item_text, rs,
};

use crate::idempotency::{
    OccurrenceTracker, encode_session_id, item_digest, item_event_key, model_change_key,
    thinking_level_change_key,
};

pub const ACTOR: &str = "grok";
pub const HARNESS: &str = "grok-build";

/// Max chars of an image URL retained in LHC text payloads.
const IMAGE_URL_PREVIEW_CHARS: usize = 128;
/// Max chars of serialized backend tool outputs in a tool_result.
const BACKEND_OUTPUTS_PREVIEW_CHARS: usize = 2048;

/// One mapped LHC event ready for `message_events`.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedEvent {
    pub input: MessageEventInput,
}

/// Host-observed facts for a `turn_end` payload (schema v5 / D1–D2).
///
/// All fields optional — empty payload remains valid for hosts that omit them.
/// Shape mirrors vendored `TurnEndPayload` optionals. G1 threads the param
/// default-empty; G2 wires the shell turn-boundary signal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnEndFacts {
    /// `"completed"` or `"aborted"`.
    pub outcome: Option<&'static str>,
    pub outcome_reason: Option<String>,
    /// Host wall-clock start (ISO-8601 UTC).
    pub started_at: Option<String>,
    /// Host wall-clock end (ISO-8601 UTC).
    pub ended_at: Option<String>,
}

impl TurnEndFacts {
    /// True when every field is absent (empty payload remains valid).
    pub fn is_empty(&self) -> bool {
        self.outcome.is_none()
            && self.outcome_reason.is_none()
            && self.started_at.is_none()
            && self.ended_at.is_none()
    }

    /// Build facts from the shell turn-end fan-out (schema v5 / G2).
    ///
    /// * `outcome_label` — wire label of [`TurnHookOutcome`](xai_tool_protocol):
    ///   `"completed"` / `"cancelled"` / `"error"`, or any unknown tail.
    /// * Fold: `completed` → `"completed"`; `cancelled` | `error` | unknown →
    ///   `"aborted"` (mirrors `handle.rs` folding unknowns to Error, then to
    ///   schema's two-valued outcome).
    /// * `outcomeReason` — `cancellation_category` when present, else the
    ///   outcome label lowercased.
    /// * `endedAt` = `ended_at` (caller stamps `SystemTime::now()` at the hook);
    ///   `startedAt` = `ended_at - duration_ms` (scout §2 option a).
    pub fn from_shell_outcome(
        outcome_label: &str,
        cancellation_category: Option<String>,
        duration_ms: u64,
        ended_at: SystemTime,
    ) -> Self {
        let label = outcome_label.to_ascii_lowercase();
        let outcome: &'static str = match label.as_str() {
            "completed" => "completed",
            // Cancelled, Error, and any non_exhaustive tail → aborted.
            _ => "aborted",
        };
        let outcome_reason = cancellation_category.unwrap_or(label);
        let started_at = ended_at
            .checked_sub(Duration::from_millis(duration_ms))
            .unwrap_or(UNIX_EPOCH);
        Self {
            outcome: Some(outcome),
            outcome_reason: Some(outcome_reason),
            started_at: Some(format_system_time_iso8601_millis(started_at)),
            ended_at: Some(format_system_time_iso8601_millis(ended_at)),
        }
    }
}

/// Format `t` as ISO-8601 UTC with millisecond precision (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
///
/// Tiny host-local formatter — no chrono dependency (scout §2).
pub fn format_system_time_iso8601_millis(t: SystemTime) -> String {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let (year, month, day, hour, minute, second) = civil_utc_from_unix_secs(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Civil UTC Y-M-D h:m:s from Unix seconds (Howard Hinnant civil_from_days).
fn civil_utc_from_unix_secs(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let day_secs = 86_400u64;
    let days = (secs / day_secs) as i64;
    let rem = secs % day_secs;
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;
    let second = (rem % 60) as u32;

    // days since 1970-01-01 → civil date
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32, hour, minute, second)
}

/// Apply facts onto an existing mapped `turn_end` event's payload (key unchanged).
pub fn apply_turn_end_facts(event: &mut MappedEvent, facts: &TurnEndFacts) {
    if event.input.event_kind != "turn_end" {
        return;
    }
    if let Some(outcome) = facts.outcome {
        event.input.payload.insert("outcome".into(), json!(outcome));
    }
    if let Some(reason) = facts.outcome_reason.as_ref() {
        event
            .input
            .payload
            .insert("outcomeReason".into(), json!(reason));
    }
    if let Some(started) = facts.started_at.as_ref() {
        event
            .input
            .payload
            .insert("startedAt".into(), json!(started));
    }
    if let Some(ended) = facts.ended_at.as_ref() {
        event.input.payload.insert("endedAt".into(), json!(ended));
    }
}

/// Host-authored `turn_end` when the shell ends a turn that never produced a
/// terminal toolless Assistant (e.g. cancelled mid-tools). Key is shell-scoped
/// so it never collides with item-mapped keys; not re-minted on bootstrap.
pub fn shell_turn_end_event(
    session_id: &str,
    turn_number: u64,
    facts: &TurnEndFacts,
) -> MappedEvent {
    let sid = encode_session_id(session_id);
    let key = format!("grok:{sid}:g0:shell_turn_end:{turn_number}");
    let mut event = MappedEvent {
        input: MessageEventInput {
            event_kind: "turn_end".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR.to_string(),
            harness: HARNESS.to_string(),
            payload: Map::new(),
            extra: Map::new(),
        },
    };
    apply_turn_end_facts(&mut event, facts);
    event
}

/// Map a single conversation item into zero or more LHC events.
///
/// `turn_end_facts` populates optional `turn_end` payload fields when this
/// item emits a `turn_end`. Facts must **not** enter the idempotency key
/// (rewind/replay dedup depends on key stability). Pass
/// [`TurnEndFacts::default`] when the host has no facts (bootstrap/replace
/// re-map, and live path until G2 wires the shell signal).
pub fn map_item(
    session_id: &str,
    generation: u64,
    item: &ConversationItem,
    tracker: &mut OccurrenceTracker,
    turn_end_facts: &TurnEndFacts,
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
                let (text, blocks) = content_parts_blocks(&user.content);
                vec![user_prompt_event(
                    session_id, generation, &digest, occ, &text, blocks,
                )]
            }
            Some(reason) => map_synthetic_user(
                session_id,
                generation,
                &digest,
                occ,
                reason,
                user,
                turn_end_facts,
            ),
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
                out.push(turn_end_event(
                    session_id,
                    generation,
                    &digest,
                    occ,
                    None,
                    turn_end_facts,
                ));
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
            // Preserve host-owned encrypted reasoning as LHC signature (R2).
            // provider/model/api identity is attached on the live capture path
            // via the side channel (never invented on bootstrap/replace re-map).
            let signature = reasoning
                .encrypted_content
                .as_deref()
                .filter(|s| !s.is_empty());
            vec![thinking_event(
                session_id, generation, &digest, occ, &text, part, signature,
            )]
        }
    }
}

/// Exhaustive SyntheticReason mapping driven by `starts_prompt_turn()`.
fn map_synthetic_user(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    reason: &SyntheticReason,
    user: &xai_grok_sampling_types::UserItem,
    turn_end_facts: &TurnEndFacts,
) -> Vec<MappedEvent> {
    let text = content_parts_text(&user.content);
    // Exhaustive on SyntheticReason — no `_ =>`.
    let starts_turn = match reason {
        // Upstream 2026-09 sync: `AgentMessage` (model-authored input from
        // another agent) is a turn start; `Unknown` flipped to a turn start
        // (fail-safe boundary for future reasons); `LengthContinue` is the
        // mid-turn continue reminder after a salvaged Length truncation.
        SyntheticReason::AgentMessage
        | SyntheticReason::Unknown
        | SyntheticReason::TaskCompleted
        | SyntheticReason::SubagentCompleted
        | SyntheticReason::NotificationDrain
        | SyntheticReason::GoalClassifierNudge
        | SyntheticReason::SchedulerFired => {
            debug_assert!(reason.starts_prompt_turn());
            true
        }
        SyntheticReason::CompactionMeta
        | SyntheticReason::SystemReminder
        | SyntheticReason::LengthContinue
        | SyntheticReason::ProjectInstructions
        | SyntheticReason::AutoContinue
        | SyntheticReason::AutoRecovery
        | SyntheticReason::Interjection
        | SyntheticReason::GoalSummary
        | SyntheticReason::StopHookFeedback
        | SyntheticReason::WorkingDirectorySwitch => {
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
            turn_end_event(
                session_id,
                generation,
                digest,
                occ,
                Some("pre_synthetic"),
                turn_end_facts,
            ),
            note,
        ]
    } else {
        vec![note]
    }
}

/// Map a full history slice; returns events and the aligned occurrence tracker.
///
/// Bootstrap / `replace_history` re-maps pass [`TurnEndFacts::default`] — live
/// host facts are not reconstructed from history (see scouting report §4.4).
pub fn map_history(
    session_id: &str,
    generation: u64,
    items: &[ConversationItem],
    turn_end_facts: &TurnEndFacts,
) -> (Vec<MappedEvent>, OccurrenceTracker) {
    let mut tracker = OccurrenceTracker::new();
    let mut out = Vec::new();
    for item in items {
        out.extend(map_item(
            session_id,
            generation,
            item,
            &mut tracker,
            turn_end_facts,
        ));
    }
    (out, tracker)
}

/// Messages API content blocks for a Grok content list (schema v13 content
/// blocks). Returns the text of the text parts (what text-only readers see)
/// and, only when an image is present, the full ordered block array: text
/// parts as `text` blocks, images as `image` blocks. A `data:<media>;base64,`
/// URL becomes a base64 source (intake moves the bytes to the blob table);
/// any other URL becomes a `url` source. Text-only content returns `None`
/// so the recorded payload is byte-identical to the pre-v13 shape.
fn content_parts_blocks(parts: &[ContentPart]) -> (String, Option<Vec<Map<String, Value>>>) {
    let has_image = parts.iter().any(|p| matches!(p, ContentPart::Image { .. }));
    let text = parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_ref().to_string()),
            ContentPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !has_image {
        return (text, None);
    }
    let blocks = parts
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => {
                let mut b = Map::new();
                b.insert("type".into(), json!("text"));
                b.insert("text".into(), json!(text.as_ref()));
                b
            }
            ContentPart::Image { url } => image_block(url.as_ref()),
        })
        .collect();
    (text, Some(blocks))
}

/// `{type:"image", source:{...}}` for one Grok image URL. Key order is the
/// Messages API order (type, media_type, data) — it is part of the persisted
/// bytes once the block is stored.
pub fn image_block(url: &str) -> Map<String, Value> {
    let mut source = Map::new();
    match parse_data_url(url) {
        Some((media_type, data)) => {
            source.insert("type".into(), json!("base64"));
            source.insert("media_type".into(), json!(media_type));
            source.insert("data".into(), json!(data));
        }
        None => {
            source.insert("type".into(), json!("url"));
            source.insert("url".into(), json!(url));
        }
    }
    let mut b = Map::new();
    b.insert("type".into(), json!("image"));
    b.insert("source".into(), Value::Object(source));
    b
}

/// `data:<media_type>;base64,<data>` → (media_type, data). Anything else
/// (plain URL, non-base64 data URL, empty media type) is `None`.
pub fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    if media_type.is_empty() {
        return None;
    }
    Some((media_type, data))
}

/// Grok image URL for one served `image` block: base64 sources come back as
/// the `data:` URL they arrived as; url sources as their URL. Inverse of
/// [`image_block`] for well-formed blocks.
pub fn image_block_url(block: &Map<String, Value>) -> Option<String> {
    let source = block.get("source")?.as_object()?;
    match source.get("type")?.as_str()? {
        "base64" => {
            let media_type = source.get("media_type")?.as_str()?;
            let data = source.get("data")?.as_str()?;
            Some(format!("data:{media_type};base64,{data}"))
        }
        "url" => Some(source.get("url")?.as_str()?.to_string()),
        _ => None,
    }
}

fn user_prompt_event(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    text: &str,
    blocks: Option<Vec<Map<String, Value>>>,
) -> MappedEvent {
    let key = item_event_key(session_id, generation, digest, occ, "user_prompt", None);
    let mut payload = text_payload(text);
    if let Some(blocks) = blocks {
        payload.insert(
            "blocks".into(),
            Value::Array(blocks.into_iter().map(Value::Object).collect()),
        );
    }
    MappedEvent {
        input: MessageEventInput {
            event_kind: "user_prompt".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR.to_string(),
            harness: HARNESS.to_string(),
            payload,
            extra: Map::new(),
        },
    }
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

/// Map `assistant_thinking` with optional opaque signature (encrypted reasoning).
///
/// Schema R2: payload is `AssistantThinkingPayload` (`text` + optional
/// `signature`). Empty signature is omitted rather than sent as `""`.
fn thinking_event(
    session_id: &str,
    generation: u64,
    digest: &str,
    occ: u64,
    text: &str,
    part: Option<&str>,
    signature: Option<&str>,
) -> MappedEvent {
    let key = item_event_key(
        session_id,
        generation,
        digest,
        occ,
        "assistant_thinking",
        part,
    );
    MappedEvent {
        input: MessageEventInput {
            event_kind: "assistant_thinking".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR.to_string(),
            harness: HARNESS.to_string(),
            payload: assistant_thinking_payload(text, signature),
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
    facts: &TurnEndFacts,
) -> MappedEvent {
    // Key stability: facts must NOT enter the key (scout §4.3 / load-bearing
    // for rewind/replay dedup).
    let key = item_event_key(session_id, generation, digest, occ, "turn_end", part);
    let mut payload = Map::new();
    if let Some(outcome) = facts.outcome {
        payload.insert("outcome".into(), json!(outcome));
    }
    if let Some(reason) = facts.outcome_reason.as_ref() {
        payload.insert("outcomeReason".into(), json!(reason));
    }
    if let Some(started) = facts.started_at.as_ref() {
        payload.insert("startedAt".into(), json!(started));
    }
    if let Some(ended) = facts.ended_at.as_ref() {
        payload.insert("endedAt".into(), json!(ended));
    }
    MappedEvent {
        input: MessageEventInput {
            event_kind: "turn_end".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR.to_string(),
            harness: HARNESS.to_string(),
            payload,
            extra: Map::new(),
        },
    }
}

/// Serialize a host `TokenUsage` into a free-form JSON object for
/// `assistant_text.providerUsage` (verbatim; no field filter).
pub fn token_usage_to_provider_usage(usage: &TokenUsage) -> Option<Map<String, Value>> {
    match serde_json::to_value(usage) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Attach optional `providerUsage` to a mapped `assistant_text` event.
pub fn attach_provider_usage(event: &mut MappedEvent, usage: &Map<String, Value>) {
    if event.input.event_kind != "assistant_text" {
        return;
    }
    event
        .input
        .payload
        .insert("providerUsage".into(), Value::Object(usage.clone()));
}

/// Attach host-observed provider/model/api to `assistant_text` or
/// `assistant_thinking`. Partial fields are allowed (omit empty). Does not
/// invent missing identity — caller supplies only observed facts.
pub fn attach_assistant_identity(
    event: &mut MappedEvent,
    identity: &xai_chat_state::HostAssistantIdentity,
) {
    if event.input.event_kind != "assistant_text" && event.input.event_kind != "assistant_thinking"
    {
        return;
    }
    if !identity.provider.is_empty() {
        event
            .input
            .payload
            .insert("provider".into(), json!(identity.provider.clone()));
    }
    if let Some(model) = identity.model.as_ref().filter(|m| !m.is_empty()) {
        event
            .input
            .payload
            .insert("model".into(), json!(model.clone()));
    }
    if let Some(api) = identity.api.as_ref().filter(|a| !a.is_empty()) {
        event.input.payload.insert("api".into(), json!(api.clone()));
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
    let content = result.content.as_ref().to_string();
    // Host ToolResultItem has no is_error field — omit rather than invent.
    let mut payload = Map::new();
    payload.insert("toolCallId".into(), json!(result.tool_call_id));
    payload.insert("content".into(), json!(content));
    // Inline images (read_file on an image/PDF) ride as content blocks after
    // the text: the text block first, then one image block per image, so the
    // served result restores `ToolResultItem.images` in order.
    if !result.images.is_empty() {
        let mut blocks: Vec<Value> = Vec::with_capacity(result.images.len() + 1);
        if !content.is_empty() {
            let mut b = Map::new();
            b.insert("type".into(), json!("text"));
            b.insert("text".into(), json!(content));
            blocks.push(Value::Object(b));
        }
        for part in &result.images {
            match part {
                ContentPart::Image { url } => blocks.push(Value::Object(image_block(url.as_ref()))),
                ContentPart::Text { text } => {
                    let mut b = Map::new();
                    b.insert("type".into(), json!("text"));
                    b.insert("text".into(), json!(text.as_ref()));
                    blocks.push(Value::Object(b));
                }
            }
        }
        payload.insert("blocks".into(), Value::Array(blocks));
    }
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

fn assistant_thinking_payload(text: &str, signature: Option<&str>) -> Map<String, Value> {
    let mut map = text_payload(text);
    if let Some(sig) = signature {
        map.insert("signature".into(), json!(sig));
    }
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

    fn empty_facts() -> TurnEndFacts {
        TurnEndFacts::default()
    }

    #[test]
    fn real_user_is_user_prompt() {
        let mut t = OccurrenceTracker::new();
        let ev = map_item(
            "s",
            0,
            &ConversationItem::user("hi"),
            &mut t,
            &empty_facts(),
        );
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
            let ev = map_item("s", 0, &item, &mut t, &empty_facts());
            assert_eq!(ev.len(), 2, "{reason:?}");
            assert_eq!(ev[0].input.event_kind, "turn_end");
            // Empty facts → empty payload (schema v5: empty stays valid).
            assert!(
                ev[0].input.payload.is_empty(),
                "default facts must omit all turn_end fields: {:?}",
                ev[0].input.payload
            );
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
            let ev = map_item("s", 0, &item, &mut t, &empty_facts());
            assert_eq!(ev.len(), 1);
            assert_eq!(ev[0].input.event_kind, "runtime_note");
        }
    }

    #[test]
    fn assistant_without_tools_emits_turn_end() {
        let mut t = OccurrenceTracker::new();
        let ev = map_item(
            "s",
            0,
            &ConversationItem::assistant("done"),
            &mut t,
            &empty_facts(),
        );
        let te = ev
            .iter()
            .find(|e| e.input.event_kind == "turn_end")
            .expect("turn_end");
        assert!(
            te.input.payload.is_empty(),
            "default facts → empty turn_end payload"
        );
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
        let ev = map_item("s", 0, &item, &mut t, &empty_facts());
        assert!(!ev.iter().any(|e| e.input.event_kind == "turn_end"));
    }

    #[test]
    fn turn_end_empty_payload_when_no_facts() {
        let event = turn_end_event("s", 0, "digest", 0, None, &TurnEndFacts::default());
        assert_eq!(event.input.event_kind, "turn_end");
        assert!(event.input.payload.is_empty());
    }

    #[test]
    fn turn_end_carries_v5_host_facts() {
        let facts = TurnEndFacts {
            outcome: Some("aborted"),
            outcome_reason: Some("interrupted".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:04.000Z".into()),
        };
        let event = turn_end_event("s", 0, "digest", 0, None, &facts);
        assert_eq!(event.input.payload.get("outcome"), Some(&json!("aborted")));
        assert_eq!(
            event.input.payload.get("outcomeReason"),
            Some(&json!("interrupted"))
        );
        assert_eq!(
            event.input.payload.get("startedAt"),
            Some(&json!("2026-07-01T12:00:00.000Z"))
        );
        assert_eq!(
            event.input.payload.get("endedAt"),
            Some(&json!("2026-07-01T12:00:04.000Z"))
        );
        assert!(event.input.extra.is_empty());
    }

    /// Facts must not enter the idempotency key — load-bearing for rewind/replay dedup.
    #[test]
    fn turn_end_facts_do_not_enter_idempotency_key() {
        let empty = turn_end_event("s", 0, "digest", 0, None, &TurnEndFacts::default());
        let facts = TurnEndFacts {
            outcome: Some("completed"),
            outcome_reason: Some("ok".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:04.000Z".into()),
        };
        let populated = turn_end_event("s", 0, "digest", 0, None, &facts);
        assert_eq!(
            empty.input.idempotency_key, populated.input.idempotency_key,
            "facts-bearing and facts-empty turn_end must share the same key"
        );
        assert!(empty.input.payload.is_empty());
        assert!(!populated.input.payload.is_empty());
    }

    #[test]
    fn provider_usage_attaches_only_to_assistant_text() {
        let mut t = OccurrenceTracker::new();
        let mut events = map_item(
            "s",
            0,
            &ConversationItem::assistant("hello"),
            &mut t,
            &empty_facts(),
        );
        assert_eq!(events[0].input.event_kind, "assistant_text");
        let usage = json!({"prompt_tokens": 11, "completion_tokens": 3})
            .as_object()
            .cloned()
            .unwrap();
        attach_provider_usage(&mut events[0], &usage);
        assert_eq!(
            events[0].input.payload.get("providerUsage"),
            Some(&Value::Object(usage.clone()))
        );
        // turn_end must not receive the attach (kind gate).
        if let Some(te) = events.iter_mut().find(|e| e.input.event_kind == "turn_end") {
            attach_provider_usage(te, &usage);
            assert!(te.input.payload.get("providerUsage").is_none());
        }
    }

    #[test]
    fn assistant_identity_attaches_to_text_and_thinking_only() {
        use xai_chat_state::HostAssistantIdentity;
        let id = HostAssistantIdentity {
            provider: "xai".into(),
            model: Some("grok-4".into()),
            api: Some("responses".into()),
        };
        let mut t = OccurrenceTracker::new();
        let mut text = map_item(
            "s",
            0,
            &ConversationItem::assistant("hi"),
            &mut t,
            &empty_facts(),
        );
        attach_assistant_identity(&mut text[0], &id);
        assert_eq!(text[0].input.payload.get("provider"), Some(&json!("xai")));
        assert_eq!(text[0].input.payload.get("model"), Some(&json!("grok-4")));
        assert_eq!(text[0].input.payload.get("api"), Some(&json!("responses")));
        if let Some(te) = text.iter_mut().find(|e| e.input.event_kind == "turn_end") {
            attach_assistant_identity(te, &id);
            assert!(te.input.payload.get("provider").is_none());
        }

        let mut r = synthesized_reasoning_item("think");
        r.encrypted_content = Some("sig".into());
        let mut think = map_item(
            "s",
            0,
            &ConversationItem::Reasoning(r),
            &mut t,
            &empty_facts(),
        );
        attach_assistant_identity(&mut think[0], &id);
        assert_eq!(think[0].input.payload.get("signature"), Some(&json!("sig")));
        assert_eq!(think[0].input.payload.get("provider"), Some(&json!("xai")));
        assert_eq!(think[0].input.payload.get("model"), Some(&json!("grok-4")));
    }

    #[test]
    fn partial_identity_omits_missing_fields() {
        use xai_chat_state::HostAssistantIdentity;
        let id = HostAssistantIdentity {
            provider: "xai".into(),
            model: None,
            api: Some("responses".into()),
        };
        let mut t = OccurrenceTracker::new();
        let mut text = map_item(
            "s",
            0,
            &ConversationItem::assistant("hi"),
            &mut t,
            &empty_facts(),
        );
        attach_assistant_identity(&mut text[0], &id);
        assert_eq!(text[0].input.payload.get("provider"), Some(&json!("xai")));
        assert!(text[0].input.payload.get("model").is_none());
        assert_eq!(text[0].input.payload.get("api"), Some(&json!("responses")));
    }

    #[test]
    fn token_usage_to_provider_usage_is_verbatim_object() {
        let usage = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            reasoning_tokens: 1,
            cached_prompt_tokens: 4,
            cache_creation_prompt_tokens: 3,
        };
        let map = token_usage_to_provider_usage(&usage).expect("object");
        assert_eq!(map.get("prompt_tokens"), Some(&json!(10)));
        assert_eq!(map.get("completion_tokens"), Some(&json!(2)));
        assert_eq!(map.get("total_tokens"), Some(&json!(12)));
        assert_eq!(map.get("reasoning_tokens"), Some(&json!(1)));
        assert_eq!(map.get("cached_prompt_tokens"), Some(&json!(4)));
        assert_eq!(map.get("cache_creation_prompt_tokens"), Some(&json!(3)));
    }

    /// Fold: Completed → completed; Cancelled|Error|unknown-tail → aborted.
    /// Mutation: map `"cancelled"` → `"completed"` and this fails.
    #[test]
    fn shell_outcome_fold_all_variants_including_unknown_tail() {
        let ended = UNIX_EPOCH + Duration::from_secs(1_720_000_000);
        let cases: &[(&str, Option<&str>, &str, &str)] = &[
            ("completed", None, "completed", "completed"),
            (
                "completed",
                Some("action_stationarity"),
                "completed",
                "action_stationarity",
            ),
            ("cancelled", None, "aborted", "cancelled"),
            (
                "cancelled",
                Some("doom_loop_repetition"),
                "aborted",
                "doom_loop_repetition",
            ),
            ("error", None, "aborted", "error"),
            // non_exhaustive tail — same fold as handle.rs unknowns → Error → aborted
            ("future_variant", None, "aborted", "future_variant"),
            ("COMPLETED", None, "completed", "completed"), // label lowercased
        ];
        for &(label, cat, want_outcome, want_reason) in cases {
            let facts =
                TurnEndFacts::from_shell_outcome(label, cat.map(str::to_string), 4_000, ended);
            assert_eq!(
                facts.outcome,
                Some(want_outcome),
                "label={label:?} cat={cat:?}"
            );
            assert_eq!(
                facts.outcome_reason.as_deref(),
                Some(want_reason),
                "label={label:?} cat={cat:?}"
            );
            assert_eq!(
                facts.ended_at.as_deref(),
                Some("2024-07-03T09:46:40.000Z"),
                "fixed epoch stamp"
            );
            assert_eq!(
                facts.started_at.as_deref(),
                Some("2024-07-03T09:46:36.000Z"),
                "startedAt = endedAt - duration_ms"
            );
        }
    }

    #[test]
    fn iso8601_formatter_utc_millis_z() {
        let t = UNIX_EPOCH + Duration::from_millis(0);
        assert_eq!(
            format_system_time_iso8601_millis(t),
            "1970-01-01T00:00:00.000Z"
        );
        let t = UNIX_EPOCH + Duration::from_millis(1_704_067_200_000 + 123);
        // 2024-01-01T00:00:00.123Z
        assert_eq!(
            format_system_time_iso8601_millis(t),
            "2024-01-01T00:00:00.123Z"
        );
        let s = format_system_time_iso8601_millis(SystemTime::now());
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(s.len(), 24, "{s}");
        assert_eq!(&s[10..11], "T");
    }

    #[test]
    fn apply_turn_end_facts_preserves_key() {
        let mut empty = turn_end_event("s", 0, "digest", 0, None, &TurnEndFacts::default());
        let key = empty.input.idempotency_key.clone();
        let facts = TurnEndFacts {
            outcome: Some("completed"),
            outcome_reason: Some("completed".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:04.000Z".into()),
        };
        apply_turn_end_facts(&mut empty, &facts);
        assert_eq!(empty.input.idempotency_key, key);
        assert_eq!(
            empty.input.payload.get("outcome"),
            Some(&json!("completed"))
        );
    }

    #[test]
    fn shell_turn_end_event_carries_facts_and_distinct_key() {
        let facts = TurnEndFacts {
            outcome: Some("aborted"),
            outcome_reason: Some("cancelled".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:04.000Z".into()),
        };
        let ev = shell_turn_end_event("sess:1", 7, &facts);
        assert_eq!(ev.input.event_kind, "turn_end");
        assert_eq!(
            ev.input.idempotency_key.as_deref(),
            Some("grok:sess%3A1:g0:shell_turn_end:7")
        );
        assert_eq!(ev.input.payload.get("outcome"), Some(&json!("aborted")));
    }

    /// Replay / bootstrap re-map never reconstructs live host facts.
    #[test]
    fn map_history_replay_stays_empty_facts() {
        let items = vec![
            ConversationItem::user("hi"),
            ConversationItem::assistant("bye"),
        ];
        let (mapped, _) = map_history("s", 0, &items, &TurnEndFacts::default());
        let te = mapped
            .iter()
            .find(|e| e.input.event_kind == "turn_end")
            .expect("turn_end");
        assert!(
            te.input.payload.is_empty(),
            "bootstrap/replace re-map must emit empty turn_end payload"
        );
    }

    #[test]
    fn reasoning_is_assistant_thinking() {
        let mut t = OccurrenceTracker::new();
        let item = ConversationItem::Reasoning(synthesized_reasoning_item("think"));
        let ev = map_item("s", 0, &item, &mut t, &empty_facts());
        assert_eq!(ev[0].input.event_kind, "assistant_thinking");
        assert_eq!(ev[0].input.payload.get("text"), Some(&json!("think")));
        assert!(
            ev[0].input.payload.get("signature").is_none(),
            "synthesized reasoning has no encrypted_content"
        );
    }

    /// R2: host `Reasoning.encrypted_content` maps to LHC `signature`.
    #[test]
    fn reasoning_encrypted_content_is_signature() {
        let mut t = OccurrenceTracker::new();
        let mut r = synthesized_reasoning_item("think");
        r.encrypted_content = Some("enc-sig-xyz".into());
        let item = ConversationItem::Reasoning(r);
        let ev = map_item("s", 0, &item, &mut t, &empty_facts());
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].input.event_kind, "assistant_thinking");
        assert_eq!(ev[0].input.payload.get("text"), Some(&json!("think")));
        assert_eq!(
            ev[0].input.payload.get("signature"),
            Some(&json!("enc-sig-xyz"))
        );
        // Identity provenance deliberately not invented here.
        assert!(ev[0].input.payload.get("provider").is_none());
        assert!(ev[0].input.payload.get("model").is_none());
        assert!(ev[0].input.payload.get("api").is_none());
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
        let (ev, mut t) = map_history("s", 0, &[ConversationItem::user("a")], &empty_facts());
        assert_eq!(ev.len(), 1);
        // Next identical item gets occ=1
        let d = item_digest(&ConversationItem::user("a"));
        assert_eq!(t.next(&d), 1);
    }
}
