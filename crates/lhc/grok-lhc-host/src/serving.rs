//! Request-context serving: LHC [`SessionThreadView`] → host `ConversationItem`s.
//!
//! Hook 4 lives in the shell (`turn.rs`) after `build_request`. This module
//! translates and validates the substitution so the shell can swap `items`
//! without touching system/tool host ownership. Replace-mode write-back also
//! builds its native body here — that body must be a **fixpoint** (see
//! [`build_writeback_conversation`]).
//!
//! **Classification source of truth:** typed [`SessionThreadView`] structure
//! plus a once-per-translation [`SourceKindIndex`] keyed by `message_id`
//! (from public `messages.list`). Never content prefixes or native
//! byte-equality.

use std::collections::HashMap;

use lhc::messages::{MessageKind, MessageRecord};
use lhc::shared_tech::view::{
    SessionAssistantPartType, SessionThreadView, SessionThreadViewEntry,
    SessionThreadViewEntrySource, SessionThreadViewMessage, SessionThreadViewRuntimeEntry,
    SessionUserMessage,
};
use tracing::{debug, warn};
use xai_grok_sampling_types::{ConversationItem, ToolCall, synthesized_reasoning_item};

/// `message_id` → recorded [`MessageKind`], fetched once per translation.
///
/// The public `SessionThreadView` collapses RuntimeNote into `Message(User)`
/// (same shape as a real prompt). Kind is recovered structurally via
/// `source_messages[].message_id` → this index (SDK `messages.list` /
/// `MessageRecord.kind`), never via content prefixes.
#[derive(Debug, Clone, Default)]
pub struct SourceKindIndex {
    by_message_id: HashMap<String, MessageKind>,
}

impl SourceKindIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_message_records(records: &[MessageRecord]) -> Self {
        let mut by_message_id = HashMap::with_capacity(records.len());
        for r in records {
            by_message_id.insert(r.message_id.clone(), r.kind);
        }
        Self { by_message_id }
    }

    pub fn insert(&mut self, message_id: impl Into<String>, kind: MessageKind) {
        self.by_message_id.insert(message_id.into(), kind);
    }

    pub fn with(mut self, message_id: impl Into<String>, kind: MessageKind) -> Self {
        self.insert(message_id, kind);
        self
    }

    /// Test helper: mark every non-band user `message_id` in `view` as
    /// [`MessageKind::UserPrompt`]. Override runtime-note ids afterward.
    pub fn assume_sourced_users_are_prompts(view: &SessionThreadView) -> Self {
        let mut idx = Self::new();
        for entry in &view.entries {
            if let SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u)) = entry
                && !u.source_messages.is_empty()
            {
                for s in &u.source_messages {
                    idx.insert(s.message_id.clone(), MessageKind::UserPrompt);
                }
            }
        }
        idx
    }

    /// Real user prompt only when every source is a known `UserPrompt`.
    ///
    /// **Fail toward synthetic:** missing id, empty sources (caller handles
    /// bands), or any non-`UserPrompt` kind ⇒ not a real prompt. A synthetic
    /// wrongly kept synthetic costs a marker slot; a synthetic wrongly
    /// promoted to real corrupts the canonical record.
    pub fn is_recorded_user_prompt(&self, sources: &[SessionThreadViewEntrySource]) -> bool {
        if sources.is_empty() {
            return false;
        }
        for s in sources {
            match self.by_message_id.get(&s.message_id) {
                Some(MessageKind::UserPrompt) => {}
                Some(_) | None => return false,
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.by_message_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_message_id.is_empty()
    }
}

/// How to materialize band entries into host items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewTranslateMode {
    /// Serving: each band stays its own `user_meta` (matches session-view shape).
    Serve,
    /// Write-back: leading contiguous bands collapse to one CompactionMeta
    /// (native `build_compacted_history` intent).
    Writeback,
}

/// Outcome of attempting to build a substituted conversation body.
#[derive(Debug)]
pub enum ServeDecision {
    /// Use these items as the full conversation body (system prefix already merged).
    Substitute { items: Vec<ConversationItem> },
    /// Keep the native `build_request` body (fail-open).
    Native { reason: &'static str },
}

/// What the last `serve_request_context` call decided for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastServeOutcome {
    pub substituted: bool,
    /// Fail-open reason when `substituted` is false.
    pub reason: Option<&'static str>,
}

fn last_serve_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, LastServeOutcome>>
{
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, LastServeOutcome>>,
    > = std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Record the serve decision used for the last turn of `session_id`.
pub fn note_last_serve(session_id: &str, decision: &ServeDecision) {
    let outcome = match decision {
        ServeDecision::Substitute { .. } => LastServeOutcome {
            substituted: true,
            reason: None,
        },
        ServeDecision::Native { reason } => LastServeOutcome {
            substituted: false,
            reason: Some(*reason),
        },
    };
    if let Ok(mut g) = last_serve_map().lock() {
        g.insert(session_id.to_string(), outcome);
    }
}

/// Last recorded serve outcome for `session_id`, if any turn consulted LHC.
pub fn last_serve_outcome(session_id: &str) -> Option<LastServeOutcome> {
    last_serve_map()
        .lock()
        .ok()
        .and_then(|g| g.get(session_id).cloned())
}

/// Evict the last-serve outcome for `session_id`.
///
/// Called on `/lhc off` and session teardown so status cannot keep claiming
/// LHC after capture is gone.
pub fn clear_last_serve_outcome(session_id: &str) {
    if let Ok(mut g) = last_serve_map().lock() {
        g.remove(session_id);
    }
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

/// Collect `UserItem::prompt_index` markers from native real-user items, in order.
pub fn native_prompt_indices(native: &[ConversationItem]) -> Vec<usize> {
    native
        .iter()
        .filter_map(|item| match item {
            ConversationItem::User(u) if u.synthetic_reason.is_none() => u.prompt_index,
            _ => None,
        })
        .collect()
}

/// Stamp real LHC user messages from the **tail** of the native index list.
///
/// **Authoritative for serving and write-back.** The LHC view's real users are
/// the surviving live turns (a suffix of the pre-compact conversation).
pub fn assign_prompt_indices_from_tail(body: &mut [ConversationItem], indices: &[usize]) {
    let mut user_slots: Vec<usize> = Vec::new();
    for (idx, item) in body.iter().enumerate() {
        if let ConversationItem::User(u) = item
            && u.synthetic_reason.is_none()
        {
            user_slots.push(idx);
        }
    }
    if user_slots.is_empty() || indices.is_empty() {
        return;
    }
    let take = user_slots.len().min(indices.len());
    let native_tail = &indices[indices.len() - take..];
    let body_tail = &user_slots[user_slots.len() - take..];
    for (slot, &pi) in body_tail.iter().zip(native_tail.iter()) {
        if let ConversationItem::User(u) = &mut body[*slot] {
            u.prompt_index = Some(pi);
        }
    }
}

/// True when `SessionUserMessage` is a band: empty `source_messages`.
///
/// Verified at vendored `session_view.rs` (`band_user_message` vs UserPrompt).
/// Classification is structural — never content byte-equality or prefixes.
pub fn is_band_user(user: &SessionUserMessage) -> bool {
    user.source_messages.is_empty()
}

/// Translate a [`SessionThreadView`] into host conversation items.
///
/// **Classify on typed structure only** — `source_messages` emptiness, entry
/// variants, and `message_id` → [`SourceKindIndex`] (recorded `MessageKind`).
/// Never on content prefixes or native byte-equality. Tool-result body text
/// may be boundary-truncated by the SDK; that must not change kinds.
///
/// | Entry | Structural rule | Host item |
/// |---|---|---|
/// | `Message(User)` empty `source_messages` | **band** (compressed prefix) | `user_meta` |
/// | `Message(User)` sources all `UserPrompt` | live-tail real prompt | `User` |
/// | `Message(User)` RuntimeNote / unknown | live-tail synthetic | `user_meta` |
/// | `Message(ToolResult)` | live-tail conserved | `ToolResult` |
/// | `Message(Assistant)` text/toolCall/thinking | live-tail conserved | `Reasoning`* + `Assistant` |
/// | `Runtime(ModelChange\|ThinkingLevelChange)` | **no host ConversationItem** | `user_meta`† |
///
/// \* Thinking parts emit `ConversationItem::Reasoning` siblings before the
/// assistant item. † Model/thinking changes have no conversation-item variant
/// and the view lacks `previous` — stay `user_meta` (SDK/host boundary).
///
/// Bands stay prose by design (compaction). Live-tail kinds round-trip.
pub fn session_view_to_items(
    view: &SessionThreadView,
    kinds: &SourceKindIndex,
    mode: ViewTranslateMode,
) -> Result<Vec<ConversationItem>, String> {
    let mut out = Vec::with_capacity(view.entries.len());
    let mut band_chunks: Vec<String> = Vec::new();

    let flush_bands = |out: &mut Vec<ConversationItem>, band_chunks: &mut Vec<String>| {
        if band_chunks.is_empty() {
            return;
        }
        match mode {
            ViewTranslateMode::Serve => {
                for b in band_chunks.drain(..) {
                    out.push(ConversationItem::user_meta(b));
                }
            }
            ViewTranslateMode::Writeback => {
                let joined = band_chunks.join("\n\n");
                band_chunks.clear();
                out.push(ConversationItem::user_meta(joined));
            }
        }
    };

    for entry in &view.entries {
        match entry {
            SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u)) => {
                if is_band_user(u) {
                    band_chunks.push(u.content.clone());
                    continue;
                }
                flush_bands(&mut out, &mut band_chunks);
                if kinds.is_recorded_user_prompt(&u.source_messages) {
                    out.push(ConversationItem::user(u.content.clone()));
                } else {
                    // RuntimeNote, unknown message_id, or non-prompt kind.
                    out.push(ConversationItem::user_meta(u.content.clone()));
                }
            }
            SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(a)) => {
                flush_bands(&mut out, &mut band_chunks);
                emit_assistant_conserved(a, &mut out);
            }
            SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(tr)) => {
                flush_bands(&mut out, &mut band_chunks);
                // Live tail: conserve ToolResult (call id + content).
                out.push(ConversationItem::tool_result(
                    tr.tool_call_id.clone(),
                    tr.content.clone(),
                ));
            }
            SessionThreadViewEntry::Runtime(SessionThreadViewRuntimeEntry::ModelChange(m)) => {
                flush_bands(&mut out, &mut band_chunks);
                // No ConversationItem variant; view has only new model (no previous).
                out.push(ConversationItem::user_meta(format!(
                    "[model change] {}/{}",
                    m.provider, m.model_id
                )));
            }
            SessionThreadViewEntry::Runtime(
                SessionThreadViewRuntimeEntry::ThinkingLevelChange(t),
            ) => {
                flush_bands(&mut out, &mut band_chunks);
                // No ConversationItem variant; view has only new level.
                out.push(ConversationItem::user_meta(format!(
                    "[thinking level change] {}",
                    t.level
                )));
            }
        }
    }
    flush_bands(&mut out, &mut band_chunks);
    Ok(out)
}

/// Emit conserved assistant parts: `Reasoning` siblings, then `Assistant`
/// with text content and real `tool_calls`.
fn emit_assistant_conserved(
    a: &lhc::shared_tech::view::SessionAssistantMessage,
    out: &mut Vec<ConversationItem>,
) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for p in &a.content {
        match p.type_ {
            SessionAssistantPartType::Text => {
                if let Some(t) = &p.text
                    && !t.is_empty()
                {
                    text_parts.push(t.clone());
                }
            }
            SessionAssistantPartType::Thinking => {
                if let Some(t) = &p.thinking {
                    out.push(ConversationItem::Reasoning(synthesized_reasoning_item(
                        t.clone(),
                    )));
                }
            }
            SessionAssistantPartType::ToolCall => {
                let name = p.tool_name.clone().unwrap_or_else(|| "unknown_tool".into());
                let id = p
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| "unknown_call".into());
                let args = p
                    .arguments
                    .as_ref()
                    .map(|m| serde_json::Value::Object(m.clone()).to_string())
                    .unwrap_or_else(|| "{}".into());
                tool_calls.push(ToolCall {
                    id: std::sync::Arc::<str>::from(id),
                    name,
                    arguments: std::sync::Arc::<str>::from(args),
                });
            }
        }
    }
    let content = text_parts.join("\n");
    if content.is_empty() && tool_calls.is_empty() {
        return;
    }
    out.push(ConversationItem::Assistant(
        xai_grok_sampling_types::AssistantItem {
            content: std::sync::Arc::<str>::from(content),
            tool_calls,
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        },
    ));
}

/// Build the host conversation for Replace-mode write-back.
///
/// **Fixpoint:** `build_writeback_conversation(body, view, kinds) == body` when
/// `body` was produced from the same `view` + `kinds` (no intervening turns).
pub fn build_writeback_conversation(
    native_before: &[ConversationItem],
    view: &SessionThreadView,
    kinds: &SourceKindIndex,
) -> Result<Vec<ConversationItem>, String> {
    let mut body = session_view_to_items(view, kinds, ViewTranslateMode::Writeback)?;
    if body.is_empty() {
        return Err("empty_lhc_context".into());
    }
    // Live-tail ToolResult / assistant tool_calls are conserved — not an error.
    let indices = native_prompt_indices(native_before);
    assign_prompt_indices_from_tail(&mut body, &indices);
    let (system_prefix, _) = split_system_prefix(native_before);
    let mut items = system_prefix;
    items.extend(body);
    Ok(items)
}

/// Serve-mode translation (bands kept separate).
pub fn session_view_to_serve_items(
    view: &SessionThreadView,
    kinds: &SourceKindIndex,
) -> Result<Vec<ConversationItem>, String> {
    session_view_to_items(view, kinds, ViewTranslateMode::Serve)
}

/// Write-back-mode translation (leading bands collapsed).
pub fn session_view_to_writeback_items(
    view: &SessionThreadView,
    kinds: &SourceKindIndex,
) -> Result<Vec<ConversationItem>, String> {
    session_view_to_items(view, kinds, ViewTranslateMode::Writeback)
}

/// True when a body contains tool-call or tool-result items.
///
/// Live-tail tool cycles are conserved through serve/write-back; this helper
/// remains for tests that assert mid-cycle native bodies are replaced
/// wholesale (all-LHC), not for rejecting tools on the substitute path.
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
pub fn decide_substitution(
    native_items: &[ConversationItem],
    view: &SessionThreadView,
    kinds: &SourceKindIndex,
) -> ServeDecision {
    let mut body = match session_view_to_serve_items(view, kinds) {
        Ok(b) => b,
        Err(err) => {
            warn!(%err, "LHC serving: session-view translation failed; native path");
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
    // Live-tail tool cycles are conserved — substitution may carry them.
    let indices = native_prompt_indices(native_items);
    assign_prompt_indices_from_tail(&mut body, &indices);
    let (system_prefix, _) = split_system_prefix(native_items);
    let mut items = system_prefix;
    items.extend(body);
    ServeDecision::Substitute { items }
}

/// Apply substitution to a request's items, leaving tools/model/params intact.
pub fn apply_serve_decision(
    native_items: Vec<ConversationItem>,
    decision: ServeDecision,
) -> (Vec<ConversationItem>, bool) {
    match decision {
        ServeDecision::Substitute { items } => (items, true),
        ServeDecision::Native { reason } => {
            debug!(reason, "LHC serving: using native conversation body");
            (native_items, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lhc::shared_tech::view::SessionThreadViewEntrySource;
    use xai_grok_sampling_types::conversation_truncate_for_prompt;

    fn src(id: &str) -> SessionThreadViewEntrySource {
        SessionThreadViewEntrySource {
            message_id: id.into(),
            idempotency_key: None,
        }
    }

    fn band(text: &str) -> SessionThreadViewEntry {
        SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
            content: text.into(),
            source_messages: Vec::new(),
        }))
    }

    fn user_prompt(text: &str, id: &str) -> SessionThreadViewEntry {
        SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
            content: text.into(),
            source_messages: vec![src(id)],
        }))
    }

    fn assistant_text(text: &str, id: &str) -> SessionThreadViewEntry {
        use lhc::shared_tech::view::{SessionAssistantMessage, SessionAssistantPart};
        SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
            SessionAssistantMessage {
                content: vec![SessionAssistantPart {
                    type_: SessionAssistantPartType::Text,
                    text: Some(text.into()),
                    thinking: None,
                    tool_call_id: None,
                    tool_name: None,
                    arguments: None,
                }],
                source_messages: vec![src(id)],
            },
        ))
    }

    fn assistant_tool_call(name: &str, args_json: &str, id: &str) -> SessionThreadViewEntry {
        use lhc::shared_tech::view::{SessionAssistantMessage, SessionAssistantPart};
        let args: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(args_json).unwrap_or_default();
        SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
            SessionAssistantMessage {
                content: vec![SessionAssistantPart {
                    type_: SessionAssistantPartType::ToolCall,
                    text: None,
                    thinking: None,
                    tool_call_id: Some("c1".into()),
                    tool_name: Some(name.into()),
                    arguments: Some(args),
                }],
                source_messages: vec![src(id)],
            },
        ))
    }

    fn tool_result(name: &str, content: &str, id: &str) -> SessionThreadViewEntry {
        use lhc::shared_tech::view::SessionToolResultMessage;
        SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(
            SessionToolResultMessage {
                tool_call_id: "c1".into(),
                tool_name: Some(name.into()),
                content: content.into(),
                is_error: None,
                source_messages: vec![src(id)],
            },
        ))
    }

    fn model_change(provider: &str, model_id: &str, id: &str) -> SessionThreadViewEntry {
        use lhc::shared_tech::view::SessionModelChangeEntry;
        SessionThreadViewEntry::Runtime(SessionThreadViewRuntimeEntry::ModelChange(
            SessionModelChangeEntry {
                provider: provider.into(),
                model_id: model_id.into(),
                source_messages: vec![src(id)],
            },
        ))
    }

    fn view(entries: Vec<SessionThreadViewEntry>) -> SessionThreadView {
        SessionThreadView {
            thread_id: "t".into(),
            entries,
        }
    }

    /// Realistic post-compact session view with typed provenance.
    fn realistic_post_compact_view() -> SessionThreadView {
        view(vec![
            band("[context · brief]\nSession goals and constraints."),
            band("[context · detailed]\nTurn-by-turn compressed history."),
            band("[context · smooth]\nNarrative bridge into the live tail."),
            user_prompt("please investigate area 9", "u1"),
            assistant_tool_call("bash", r#"{"cmd":"ls"}"#, "a1"),
            tool_result("bash", "file_a\nfile_b", "tr1"),
            // SDK collapses RuntimeNote → User with sources; kind recovered via index.
            user_prompt("[runtime note] cwd switched", "rn1"),
            model_change("xai", "grok-4", "mc1"),
            user_prompt("live-2", "u2"),
            assistant_text("a2", "a2"),
        ])
    }

    fn realistic_kinds(view: &SessionThreadView) -> SourceKindIndex {
        SourceKindIndex::assume_sourced_users_are_prompts(view)
            .with("rn1", lhc::messages::MessageKind::RuntimeNote)
    }

    fn native_three_turn_tool_session() -> Vec<ConversationItem> {
        use xai_grok_sampling_types::ToolCall;
        let mut u0 = ConversationItem::user("old-0");
        u0.set_prompt_index(0);
        let mut u1 = ConversationItem::user("please investigate area 9");
        u1.set_prompt_index(1);
        let mut u2 = ConversationItem::user("live-2");
        u2.set_prompt_index(2);
        let tool_asst = ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{\"cmd\":\"ls\"}".into(),
        }]);
        vec![
            ConversationItem::system("sys"),
            u0,
            ConversationItem::assistant("a0"),
            u1,
            tool_asst,
            ConversationItem::tool_result("c1", "file_a\nfile_b"),
            ConversationItem::assistant("done looking"),
            u2,
            ConversationItem::assistant("a2"),
        ]
    }

    #[test]
    fn preserves_system_prefix_and_drops_native_body() {
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("old"),
            ConversationItem::assistant("old-a"),
        ];
        let v = view(vec![
            user_prompt("new-u", "u"),
            assistant_text("new-a", "a"),
        ]);
        match decide_substitution(
            &native,
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        ) {
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
        let v = view(vec![user_prompt("hi", "u")]);
        let body =
            session_view_to_serve_items(&v, &SourceKindIndex::assume_sourced_users_are_prompts(&v))
                .unwrap();
        assert!(!body_has_tool_cycle(&body));
    }

    #[test]
    fn empty_context_fails_open() {
        let native = vec![ConversationItem::user("u")];
        let v = view(vec![]);
        assert!(matches!(
            decide_substitution(
                &native,
                &v,
                &SourceKindIndex::assume_sourced_users_are_prompts(&v),
            ),
            ServeDecision::Native {
                reason: "empty_lhc_context"
            }
        ));
    }

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
        let v = view(vec![user_prompt("see [image omitted]", "u")]);
        match decide_substitution(
            &native,
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        ) {
            ServeDecision::Substitute { items } => {
                assert!(matches!(&items[0], ConversationItem::System(_)));
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

    #[test]
    fn accounting_memory_reminder_preserved_in_system_prefix() {
        let native = vec![
            ConversationItem::system("base\nRemember: user likes rust"),
            ConversationItem::user("old"),
        ];
        let v = view(vec![user_prompt("new", "u")]);
        match decide_substitution(
            &native,
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        ) {
            ServeDecision::Substitute { items } => match &items[0] {
                ConversationItem::System(s) => {
                    assert!(s.content.contains("Remember: user likes rust"));
                }
                other => panic!("expected system prefix, got {other:?}"),
            },
            ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
        }
    }

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
        ];
        assert!(body_has_tool_cycle(&native[1..]));
        let v = view(vec![
            user_prompt("run", "u"),
            assistant_text("calling", "a"),
        ]);
        match decide_substitution(
            &native,
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        ) {
            ServeDecision::Substitute { items } => {
                assert!(!body_has_tool_cycle(&items));
                assert!(matches!(&items[0], ConversationItem::System(_)));
            }
            ServeDecision::Native { reason } => panic!("expected substitute, got {reason}"),
        }
    }

    #[test]
    fn prompt_index_mapping_survives_substitute_and_rewind() {
        let mut u0 = ConversationItem::user("first");
        u0.set_prompt_index(0);
        let mut u1 = ConversationItem::user("second");
        u1.set_prompt_index(1);
        let native = vec![
            ConversationItem::system("sys"),
            u0,
            ConversationItem::assistant("a0"),
            u1,
            ConversationItem::assistant("a1"),
        ];
        let v = view(vec![
            user_prompt("first", "u0"),
            assistant_text("a0", "a0"),
            user_prompt("second", "u1"),
            assistant_text("a1", "a1"),
        ]);
        let ServeDecision::Substitute { items } = decide_substitution(
            &native,
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        ) else {
            panic!("expected substitute");
        };
        let markers: Vec<Option<usize>> = items
            .iter()
            .filter_map(|i| match i {
                ConversationItem::User(u) if u.synthetic_reason.is_none() => Some(u.prompt_index),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec![Some(0), Some(1)]);

        let cut = conversation_truncate_for_prompt(&items, 1);
        let kept = &items[cut..];
        let users: Vec<_> = kept
            .iter()
            .filter_map(|i| match i {
                ConversationItem::User(u) if u.synthetic_reason.is_none() => Some(u.prompt_index),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec![Some(1)]);
    }

    #[test]
    fn prompt_index_mapping_survives_fork_cut() {
        let mut u0 = ConversationItem::user("root");
        u0.set_prompt_index(0);
        let mut u1 = ConversationItem::user("branch");
        u1.set_prompt_index(1);
        let native = vec![
            ConversationItem::system("sys"),
            u0,
            ConversationItem::assistant("a"),
            u1,
        ];
        let v = view(vec![
            user_prompt("root", "u0"),
            assistant_text("a", "a"),
            user_prompt("branch", "u1"),
        ]);
        let ServeDecision::Substitute { items } = decide_substitution(
            &native,
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        ) else {
            panic!("expected substitute");
        };
        let cut = conversation_truncate_for_prompt(&items, 0);
        let first_user = items[cut..].iter().find_map(|i| match i {
            ConversationItem::User(u) if u.synthetic_reason.is_none() => Some(u.prompt_index),
            _ => None,
        });
        assert_eq!(first_user, Some(Some(0)));
    }

    #[test]
    fn serving_skips_bands_for_prompt_index() {
        let native = native_three_turn_tool_session();
        let v = realistic_post_compact_view();
        let kinds = realistic_kinds(&v);
        let ServeDecision::Substitute { items } = decide_substitution(&native, &v, &kinds) else {
            panic!("expected substitute");
        };
        let real: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                ConversationItem::User(u) if u.synthetic_reason.is_none() => u.prompt_index,
                _ => None,
            })
            .collect();
        assert_eq!(
            real,
            vec![1, 2],
            "post-compact serving must stamp live (tail) markers on the two real users"
        );
    }

    /// Band-shaped bytes with non-empty sources are a real prompt (typed), not meta.
    #[test]
    fn writeback_preserves_real_user_matching_band_prefix() {
        const PASTE: &str = "[context · brief]\nSession goals and constraints.";
        let mut u0 = ConversationItem::user("old");
        u0.set_prompt_index(0);
        let mut pasted = ConversationItem::user(PASTE);
        pasted.set_prompt_index(1);
        let mut live = ConversationItem::user("live-2");
        live.set_prompt_index(2);
        let native = vec![
            ConversationItem::system("sys"),
            u0,
            ConversationItem::assistant("a0"),
            pasted,
            ConversationItem::assistant("ack"),
            live,
        ];
        let v = view(vec![
            band("[context · brief]\nDifferent band bytes now."),
            user_prompt("please investigate area 9", "u1"),
            assistant_text("looking", "a1"),
            user_prompt(PASTE, "paste"),
            assistant_text("ack", "a2"),
            user_prompt("live-2", "u2"),
        ]);
        let items = build_writeback_conversation(
            &native,
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        )
        .expect("writeback");
        let pasted_item = items
            .iter()
            .find(|i| i.text_content() == PASTE)
            .expect("paste must survive write-back");
        match pasted_item {
            ConversationItem::User(u) => {
                assert!(
                    u.synthetic_reason.is_none(),
                    "typed real user must not become CompactionMeta"
                );
                assert_eq!(u.prompt_index, Some(1));
            }
            other => panic!("expected real User, got {other:?}"),
        }
    }

    #[test]
    fn band_is_band_by_empty_sources_not_prefix() {
        // Empty sources ⇒ band even without consulting native texts.
        let v = view(vec![band("[context · brief]\nA"), user_prompt("live", "u")]);
        let items = session_view_to_writeback_items(
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        )
        .unwrap();
        assert!(matches!(
            &items[0],
            ConversationItem::User(u) if u.synthetic_reason.is_some()
        ));
        assert!(matches!(
            &items[1],
            ConversationItem::User(u) if u.synthetic_reason.is_none()
        ));
    }

    #[test]
    fn tool_result_is_conserved_as_tool_result_item() {
        let v = view(vec![tool_result("bash", "out", "tr")]);
        let items =
            session_view_to_serve_items(&v, &SourceKindIndex::assume_sourced_users_are_prompts(&v))
                .unwrap();
        match &items[0] {
            ConversationItem::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "c1");
                assert_eq!(tr.content.as_ref(), "out");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn assistant_tool_call_is_conserved_on_assistant_item() {
        let v = view(vec![assistant_tool_call("bash", r#"{"cmd":"ls"}"#, "a1")]);
        let items =
            session_view_to_serve_items(&v, &SourceKindIndex::assume_sourced_users_are_prompts(&v))
                .unwrap();
        match &items[0] {
            ConversationItem::Assistant(a) => {
                assert_eq!(a.tool_calls.len(), 1);
                assert_eq!(a.tool_calls[0].name, "bash");
                assert_eq!(a.tool_calls[0].id.as_ref(), "c1");
            }
            other => panic!("expected Assistant with tool_calls, got {other:?}"),
        }
    }

    #[test]
    fn assistant_thinking_is_conserved_as_reasoning_sibling() {
        use lhc::shared_tech::view::{SessionAssistantMessage, SessionAssistantPart};
        let v = view(vec![SessionThreadViewEntry::Message(
            SessionThreadViewMessage::Assistant(SessionAssistantMessage {
                content: vec![
                    SessionAssistantPart {
                        type_: SessionAssistantPartType::Thinking,
                        text: None,
                        thinking: Some("plan".into()),
                        tool_call_id: None,
                        tool_name: None,
                        arguments: None,
                    },
                    SessionAssistantPart {
                        type_: SessionAssistantPartType::Text,
                        text: Some("ok".into()),
                        thinking: None,
                        tool_call_id: None,
                        tool_name: None,
                        arguments: None,
                    },
                ],
                source_messages: vec![src("a")],
            }),
        )]);
        let items =
            session_view_to_serve_items(&v, &SourceKindIndex::assume_sourced_users_are_prompts(&v))
                .unwrap();
        assert!(
            matches!(&items[0], ConversationItem::Reasoning(_)),
            "thinking part must emit Reasoning sibling"
        );
        assert!(
            matches!(&items[1], ConversationItem::Assistant(a) if a.content.as_ref() == "ok"),
            "text part must remain Assistant"
        );
    }

    /// N1: classifier must ignore tool-result body truncation at the view
    /// boundary. Same structure + different content ⇒ same kinds, real-user
    /// set, and `prompt_index` assignment. Would fail if classification keyed
    /// on content equality with native tool bytes.
    #[test]
    fn classification_insensitive_to_tool_result_boundary_truncation() {
        let mut u0 = ConversationItem::user("old-0");
        u0.set_prompt_index(0);
        let mut u1 = ConversationItem::user("please investigate area 9");
        u1.set_prompt_index(1);
        let mut u2 = ConversationItem::user("live-2");
        u2.set_prompt_index(2);
        let native = vec![
            ConversationItem::system("sys"),
            u0,
            ConversationItem::assistant("a0"),
            u1,
            ConversationItem::assistant("looking"),
            u2,
            ConversationItem::assistant("a2"),
        ];

        let full_tool = "x".repeat(800); // above FALLBACK_TRUNCATION_LIMIT (500)
        let truncated_tool = format!("{}… [truncated {} chars]", "x".repeat(500), 800 - 500);

        let full_view = view(vec![
            band("[context · brief]\nSummary."),
            user_prompt("please investigate area 9", "u1"),
            assistant_tool_call("bash", r#"{"cmd":"ls"}"#, "a1"),
            tool_result("bash", &full_tool, "tr1"),
            user_prompt("live-2", "u2"),
            assistant_text("a2", "a2"),
        ]);
        let trunc_view = view(vec![
            band("[context · brief]\nSummary."),
            user_prompt("please investigate area 9", "u1"),
            assistant_tool_call("bash", r#"{"cmd":"ls"}"#, "a1"),
            tool_result("bash", &truncated_tool, "tr1"),
            user_prompt("live-2", "u2"),
            assistant_text("a2", "a2"),
        ]);

        let full_items = build_writeback_conversation(
            &native,
            &full_view,
            &SourceKindIndex::assume_sourced_users_are_prompts(&full_view),
        )
        .expect("full");
        let trunc_items = build_writeback_conversation(
            &native,
            &trunc_view,
            &SourceKindIndex::assume_sourced_users_are_prompts(&trunc_view),
        )
        .expect("trunc");

        let kind_fp = |items: &[ConversationItem]| -> Vec<&'static str> {
            items
                .iter()
                .map(|i| match i {
                    ConversationItem::System(_) => "sys",
                    ConversationItem::User(u) if u.synthetic_reason.is_some() => "meta",
                    ConversationItem::User(_) => "user",
                    ConversationItem::Assistant(_) => "asst",
                    ConversationItem::ToolResult(_) => "tool",
                    ConversationItem::BackendToolCall(_) => "btcall",
                    ConversationItem::Reasoning(_) => "reason",
                })
                .collect()
        };
        assert_eq!(
            kind_fp(&full_items),
            kind_fp(&trunc_items),
            "entry kinds must not depend on tool-result body length"
        );
        assert_eq!(
            native_prompt_indices(&full_items),
            native_prompt_indices(&trunc_items)
        );
        assert_eq!(native_prompt_indices(&full_items), vec![1, 2]);

        let real = |items: &[ConversationItem]| -> Vec<String> {
            items
                .iter()
                .filter_map(|i| match i {
                    ConversationItem::User(u) if u.synthetic_reason.is_none() => {
                        Some(i.text_content())
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(real(&full_items), real(&trunc_items));
        assert_eq!(
            real(&full_items),
            vec![
                "please investigate area 9".to_string(),
                "live-2".to_string()
            ]
        );

        // Discriminator: if someone classifies by matching native tool bytes,
        // truncated content would diverge into a different kind/slot count.
        assert_ne!(
            full_tool, truncated_tool,
            "fixture invalid: truncation must change bytes"
        );
        assert!(
            full_items
                .iter()
                .any(|i| matches!(i, ConversationItem::ToolResult(_))),
            "tool result must remain ToolResult under full content"
        );
        assert!(
            trunc_items
                .iter()
                .any(|i| matches!(i, ConversationItem::ToolResult(_))),
            "tool result must remain ToolResult under truncated content"
        );
    }

    /// RuntimeNote is Message(User) on the typed view; kind via message_id index
    /// classifies it synthetic. Missing kind ⇒ also synthetic (fail-safe).
    #[test]
    fn runtime_note_classifies_synthetic_via_message_id_kind() {
        let v = view(vec![user_prompt("[runtime note] cwd switched", "rn1")]);
        let as_note = SourceKindIndex::new().with("rn1", lhc::messages::MessageKind::RuntimeNote);
        let items = session_view_to_serve_items(&v, &as_note).unwrap();
        match &items[0] {
            ConversationItem::User(u) => {
                assert!(
                    u.synthetic_reason.is_some(),
                    "RuntimeNote message_id must classify as user_meta"
                );
            }
            other => panic!("expected user_meta, got {other:?}"),
        }
        // Fail toward synthetic when the index has no entry.
        let unknown = SourceKindIndex::new();
        let items = session_view_to_serve_items(&v, &unknown).unwrap();
        assert!(
            matches!(&items[0], ConversationItem::User(u) if u.synthetic_reason.is_some()),
            "lookup miss must not promote to real user"
        );
    }

    #[test]
    fn runtime_notes_consume_no_prompt_index_slot() {
        let native = native_three_turn_tool_session();
        let v = realistic_post_compact_view();
        let kinds = realistic_kinds(&v);
        let items = build_writeback_conversation(&native, &v, &kinds).expect("writeback");
        assert_eq!(native_prompt_indices(&items), vec![1, 2]);
        let real: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                ConversationItem::User(u) if u.synthetic_reason.is_none() => Some(i.text_content()),
                _ => None,
            })
            .collect();
        assert_eq!(
            real,
            vec![
                "please investigate area 9".to_string(),
                "live-2".to_string()
            ]
        );
        assert!(
            items.iter().any(|i| {
                matches!(i, ConversationItem::User(u) if u.synthetic_reason.is_some())
                    && i.text_content().contains("cwd switched")
            }),
            "runtime note must write back as user_meta"
        );
    }

    #[test]
    fn writeback_native_prompt_indices_are_live_real_users_only() {
        let native = native_three_turn_tool_session();
        let v = realistic_post_compact_view();
        let kinds = realistic_kinds(&v);
        let items = build_writeback_conversation(&native, &v, &kinds).expect("writeback");
        let indices = native_prompt_indices(&items);
        assert_eq!(indices, vec![1, 2]);
        let real_users: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                ConversationItem::User(u) if u.synthetic_reason.is_none() => Some(i.text_content()),
                _ => None,
            })
            .collect();
        assert_eq!(
            real_users,
            vec![
                "please investigate area 9".to_string(),
                "live-2".to_string()
            ]
        );
    }

    fn writeback_fingerprint(items: &[ConversationItem]) -> String {
        items
            .iter()
            .map(|i| {
                let kind = match i {
                    ConversationItem::System(_) => "sys",
                    ConversationItem::User(u) => {
                        if u.synthetic_reason.is_some() {
                            "meta"
                        } else {
                            "user"
                        }
                    }
                    ConversationItem::Assistant(_) => "asst",
                    ConversationItem::ToolResult(_) => "tool",
                    ConversationItem::BackendToolCall(_) => "btcall",
                    ConversationItem::Reasoning(_) => "reason",
                };
                let pi = match i {
                    ConversationItem::User(u) => format!("{:?}", u.prompt_index),
                    _ => "-".into(),
                };
                format!("{kind}:{pi}:{}", i.text_content())
            })
            .collect::<Vec<_>>()
            .join("||")
    }

    #[test]
    fn writeback_body_is_fixpoint() {
        let native = native_three_turn_tool_session();
        let v = realistic_post_compact_view();
        let kinds = realistic_kinds(&v);
        let body1 = build_writeback_conversation(&native, &v, &kinds).expect("wb1");
        let body2 = build_writeback_conversation(&body1, &v, &kinds).expect("wb2");
        assert_eq!(
            writeback_fingerprint(&body1),
            writeback_fingerprint(&body2),
            "write-back must be a fixpoint"
        );
    }

    #[test]
    fn writeback_prompt_index_survives_rewind() {
        let native = native_three_turn_tool_session();
        let v = realistic_post_compact_view();
        let kinds = realistic_kinds(&v);
        let items = build_writeback_conversation(&native, &v, &kinds).expect("writeback");
        let markers: Vec<Option<usize>> = items
            .iter()
            .filter_map(|i| match i {
                ConversationItem::User(u) if u.synthetic_reason.is_none() => Some(u.prompt_index),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec![Some(1), Some(2)]);
        let cut = conversation_truncate_for_prompt(&items, 2);
        let kept_users: Vec<_> = items[cut..]
            .iter()
            .filter_map(|i| match i {
                ConversationItem::User(u) if u.synthetic_reason.is_none() => Some(u.prompt_index),
                _ => None,
            })
            .collect();
        assert_eq!(kept_users, vec![Some(2)]);
    }

    #[test]
    fn writeback_prompt_index_survives_fork() {
        let mut u0 = ConversationItem::user("root");
        u0.set_prompt_index(5);
        let mut u1 = ConversationItem::user("leaf");
        u1.set_prompt_index(6);
        let native = vec![
            ConversationItem::system("sys"),
            u0,
            ConversationItem::assistant("a"),
            u1,
        ];
        let v = view(vec![
            band("[context · brief]\nprior work"),
            user_prompt("root", "u0"),
            assistant_text("a", "a"),
            user_prompt("leaf", "u1"),
        ]);
        let items = build_writeback_conversation(
            &native,
            &v,
            &SourceKindIndex::assume_sourced_users_are_prompts(&v),
        )
        .expect("writeback");
        let cut = conversation_truncate_for_prompt(&items, 5);
        let first = items[cut..].iter().find_map(|i| match i {
            ConversationItem::User(u) if u.synthetic_reason.is_none() => Some(u.prompt_index),
            _ => None,
        });
        assert_eq!(first, Some(Some(5)));
    }

    #[test]
    fn writeback_bands_collapse_to_one_compaction_meta() {
        let native = native_three_turn_tool_session();
        let v = realistic_post_compact_view();
        let kinds = realistic_kinds(&v);
        let items = build_writeback_conversation(&native, &v, &kinds).expect("writeback");
        let metas: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                ConversationItem::User(u)
                    if u.synthetic_reason
                        == Some(xai_grok_sampling_types::SyntheticReason::CompactionMeta) =>
                {
                    Some(i.text_content())
                }
                _ => None,
            })
            .collect();
        assert!(
            metas.iter().any(|t| t.contains("[context · brief]")
                && t.contains("[context · detailed]")
                && t.contains("[context · smooth]")),
            "leading bands must collapse into one CompactionMeta, got {metas:?}"
        );
        assert!(
            !metas.iter().any(|t| t.contains("[tool result")),
            "live-tail tool results must not be flattened into CompactionMeta"
        );
        assert!(
            items
                .iter()
                .any(|i| matches!(i, ConversationItem::ToolResult(_))),
            "live-tail ToolResult must be conserved as ToolResult"
        );
    }
}
