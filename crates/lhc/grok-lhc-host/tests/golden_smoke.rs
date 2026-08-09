//! Golden event transcripts — layer 3 of `scripts/check-lhc-hooks.sh`.
//!
//! Fixtures are hand-authored (or refreshed only under `UPDATE_GOLDENS=1`).
//! CI never sets that env var. Projection includes `idempotencyKey`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use grok_lhc_host::{TurnEndFacts, map_history, map_model_change};
use lhc::intake_stream::{
    AssistantTextPayload, AssistantThinkingPayload, ModelChangePayload, TextPayload,
    ThinkingLevelChangePayload, ToolCallPayload, ToolResultPayload, TurnEndPayload,
};
use serde_json::{Map, Value, json};
use xai_grok_sampling_types::{
    BackendToolCallItem, BackendToolKind, ContentPart, ConversationItem, SyntheticReason, ToolCall,
    UserItem, rs, synthesized_reasoning_item,
};

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/goldens")
}

fn project(events: &[grok_lhc_host::MappedEvent]) -> Value {
    json!(
        events
            .iter()
            .map(|e| {
                json!({
                    "eventKind": e.input.event_kind,
                    "idempotencyKey": e.input.idempotency_key,
                    "actor": e.input.actor,
                    "harness": e.input.harness,
                    "payload": e.input.payload,
                })
            })
            .collect::<Vec<_>>()
    )
}

fn write_golden_if_requested(name: &str, events: &[grok_lhc_host::MappedEvent]) {
    // Explicit opt-in only — CI must never set UPDATE_GOLDENS.
    if std::env::var("UPDATE_GOLDENS").ok().as_deref() != Some("1") {
        return;
    }
    let path = goldens_dir().join(name);
    let arr = project(events);
    fs::write(path, serde_json::to_string_pretty(&arr).unwrap() + "\n").unwrap();
}

fn assert_golden(name: &str, events: &[grok_lhc_host::MappedEvent]) {
    write_golden_if_requested(name, events);
    let path = goldens_dir().join(name);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing golden {name}: {e}"));
    let expected: Value = serde_json::from_str(&raw).expect("golden json");
    let actual = project(events);
    assert_eq!(actual, expected, "golden mismatch: {name}");
}

/// Independent payload-shape anchor: each mapped payload must decode as the
/// corresponding LHC `deny_unknown_fields` type.
fn assert_lhc_payload_shapes(events: &[grok_lhc_host::MappedEvent]) {
    for e in events {
        let v = Value::Object(e.input.payload.clone());
        match e.input.event_kind.as_str() {
            // assistant_text is schema-v5 `AssistantTextPayload` (optional
            // providerUsage + optional provenance); shared TextPayload is
            // deny_unknown_fields and panics when usage is present.
            "assistant_text" => {
                let _: AssistantTextPayload = serde_json::from_value(v)
                    .unwrap_or_else(|err| panic!("assistant_text payload decode failed: {err}"));
            }
            // R2: assistant_thinking is AssistantThinkingPayload (optional
            // signature + provenance), no longer plain TextPayload.
            "assistant_thinking" => {
                let _: AssistantThinkingPayload = serde_json::from_value(v).unwrap_or_else(|err| {
                    panic!("assistant_thinking payload decode failed: {err}")
                });
            }
            "user_prompt" | "runtime_note" => {
                let _: TextPayload = serde_json::from_value(v).unwrap_or_else(|err| {
                    panic!("{} payload decode failed: {err}", e.input.event_kind)
                });
            }
            "model_change" => {
                let _: ModelChangePayload = serde_json::from_value(v)
                    .unwrap_or_else(|err| panic!("model_change decode: {err}"));
            }
            "thinking_level_change" => {
                let _: ThinkingLevelChangePayload = serde_json::from_value(v)
                    .unwrap_or_else(|err| panic!("thinking_level_change decode: {err}"));
            }
            "tool_call" => {
                let _: ToolCallPayload = serde_json::from_value(v)
                    .unwrap_or_else(|err| panic!("tool_call decode: {err}"));
            }
            "tool_result" => {
                let _: ToolResultPayload = serde_json::from_value(v)
                    .unwrap_or_else(|err| panic!("tool_result decode: {err}"));
            }
            "turn_end" => {
                let _: TurnEndPayload = serde_json::from_value(v)
                    .unwrap_or_else(|err| panic!("turn_end decode: {err}"));
            }
            other => panic!("unknown event kind in golden projection: {other}"),
        }
    }
}

fn backend_web_search() -> ConversationItem {
    ConversationItem::BackendToolCall(BackendToolCallItem {
        kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
            id: "ws_golden_1".into(),
            status: rs::WebSearchToolCallStatus::Completed,
            action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                query: "capybara".into(),
                sources: Some(vec![]),
            }),
        }),
    })
}

fn backend_x_search() -> ConversationItem {
    // CustomToolCall is #[non_exhaustive] — construct via serde.
    let ct: rs::CustomToolCall = serde_json::from_value(json!({
        "id": "xs_golden_1",
        "call_id": "call_xs_1",
        "name": "x_search",
        "input": "from:xai",
    }))
    .expect("CustomToolCall fixture");
    ConversationItem::BackendToolCall(BackendToolCallItem {
        kind: BackendToolKind::XSearch(ct),
    })
}

fn backend_code_interpreter() -> ConversationItem {
    ConversationItem::BackendToolCall(BackendToolCallItem {
        kind: BackendToolKind::CodeInterpreter(rs::CodeInterpreterToolCall {
            id: "ci_golden_1".into(),
            container_id: "ctr_1".into(),
            code: Some("print(1+1)".into()),
            outputs: Some(vec![rs::CodeInterpreterToolCallOutput::Logs(
                rs::CodeInterpreterOutputLogs { logs: "2\n".into() },
            )]),
            status: rs::CodeInterpreterToolCallStatus::Completed,
        }),
    })
}

fn user_with_image() -> ConversationItem {
    ConversationItem::User(UserItem {
        content: vec![
            ContentPart::Text {
                text: "see attached".into(),
            },
            ContentPart::Image {
                url: "https://example.com/a.png".into(),
            },
        ],
        synthetic_reason: None,
        cwd_generation: None,
        prior_turn_interrupt: None,
        prompt_index: None,
    })
} // ContentPart text/url accept Into<Arc<str>>

#[test]
fn golden_covers_every_event_kind_and_mapping_row() {
    let system = ConversationItem::system("be helpful");
    let user = ConversationItem::user("hello world");
    let imaged = user_with_image();
    let mut synthetic = ConversationItem::user("continue");
    if let ConversationItem::User(u) = &mut synthetic {
        u.synthetic_reason = Some(SyntheticReason::AutoContinue);
    }
    let mut wake = ConversationItem::user("task done");
    if let ConversationItem::User(u) = &mut wake {
        u.synthetic_reason = Some(SyntheticReason::TaskCompleted);
    }
    let assistant = ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
        content: "calling tool".into(),
        tool_calls: vec![ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        }],
        model_id: None,
        model_fingerprint: None,
        reasoning_effort: None,
    });
    let tool_result = ConversationItem::tool_result("call-1", "file contents");
    let final_assistant = ConversationItem::assistant("done");
    let reasoning = ConversationItem::Reasoning(synthesized_reasoning_item("pondering"));

    let history = vec![
        system,
        user,
        imaged,
        synthetic,
        wake,
        reasoning,
        assistant,
        tool_result,
        backend_web_search(),
        backend_x_search(),
        backend_code_interpreter(),
        final_assistant,
        ConversationItem::working_directory_switch("/tmp/moved", 1),
    ];

    let (mapped, _) = map_history("golden-session", 0, &history, &TurnEndFacts::default());
    assert_lhc_payload_shapes(&mapped);

    let mut kinds: BTreeSet<_> = mapped.iter().map(|e| e.input.event_kind.clone()).collect();

    let (model_ev, _) = map_model_change("golden-session", "model-a", "model-b", "none", "high", 0);
    assert_lhc_payload_shapes(&model_ev);
    for e in &model_ev {
        kinds.insert(e.input.event_kind.clone());
    }

    // Every mapping-table row must appear: all 9 EVENT_KINDS, plus backend
    // tool_call/tool_result pairs and an image-bearing user_prompt.
    let expected_kinds: BTreeSet<_> = [
        "user_prompt",
        "assistant_text",
        "assistant_thinking",
        "runtime_note",
        "model_change",
        "thinking_level_change",
        "tool_call",
        "tool_result",
        "turn_end",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(kinds, expected_kinds, "goldens must cover all EVENT_KINDS");

    assert!(
        mapped.iter().any(|e| {
            e.input.event_kind == "user_prompt"
                && e.input
                    .payload
                    .get("text")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.contains("[image:"))
        }),
        "must cover ContentPart::Image"
    );
    for name in ["web_search", "x_search", "code_interpreter"] {
        assert!(
            mapped.iter().any(|e| {
                e.input.event_kind == "tool_call"
                    && e.input.payload.get("toolName").and_then(|t| t.as_str()) == Some(name)
            }),
            "missing BackendToolKind arm {name}"
        );
        assert!(
            mapped.iter().any(|e| {
                e.input.event_kind == "tool_result"
                    && e.input
                        .payload
                        .get("toolCallId")
                        .and_then(|t| t.as_str())
                        .is_some_and(|id| match name {
                            "web_search" => id.starts_with("ws_"),
                            "x_search" => id.starts_with("xs_"),
                            "code_interpreter" => id.starts_with("ci_"),
                            _ => false,
                        })
            }),
            "missing paired tool_result for {name}"
        );
    }

    // Keys must be present and unique in the projection.
    let keys: Vec<_> = mapped
        .iter()
        .filter_map(|e| e.input.idempotency_key.clone())
        .collect();
    assert_eq!(keys.len(), mapped.len());
    assert_eq!(keys.iter().collect::<BTreeSet<_>>().len(), keys.len());

    assert_golden("full_turn.json", &mapped);
    assert_golden("model_thinking_change.json", &model_ev);
}

#[test]
fn golden_every_synthetic_reason() {
    let reasons = [
        SyntheticReason::CompactionMeta,
        SyntheticReason::SystemReminder,
        SyntheticReason::ProjectInstructions,
        SyntheticReason::AutoContinue,
        SyntheticReason::AutoRecovery,
        SyntheticReason::Interjection,
        SyntheticReason::TaskCompleted,
        SyntheticReason::SubagentCompleted,
        SyntheticReason::NotificationDrain,
        SyntheticReason::GoalSummary,
        SyntheticReason::GoalClassifierNudge,
        SyntheticReason::SchedulerFired,
        SyntheticReason::StopHookFeedback,
        SyntheticReason::WorkingDirectorySwitch,
        SyntheticReason::Unknown,
    ];
    let mut items = Vec::new();
    for (i, reason) in reasons.into_iter().enumerate() {
        let mut item = ConversationItem::user(format!("synthetic-{i}"));
        if let ConversationItem::User(u) = &mut item {
            u.synthetic_reason = Some(reason);
        }
        items.push(item);
    }
    let (mapped, _) = map_history("synth-session", 0, &items, &TurnEndFacts::default());
    // 5 turn-starters → turn_end+note; 10 plain notes → 20 events.
    assert_eq!(mapped.len(), 20);
    let turn_ends = mapped
        .iter()
        .filter(|e| e.input.event_kind == "turn_end")
        .count();
    assert_eq!(turn_ends, 5);
    assert!(
        mapped
            .iter()
            .filter(|e| e.input.event_kind != "turn_end")
            .all(|e| e.input.event_kind == "runtime_note")
    );
    assert_lhc_payload_shapes(&mapped);
    assert_golden("synthetic_reasons.json", &mapped);
}

#[test]
fn map_item_idempotency_keys_stable_across_replay() {
    let item = ConversationItem::user("stable");
    let (a, _) = map_history(
        "s",
        0,
        std::slice::from_ref(&item),
        &TurnEndFacts::default(),
    );
    let (b, _) = map_history(
        "s",
        0,
        std::slice::from_ref(&item),
        &TurnEndFacts::default(),
    );
    assert_eq!(
        a[0].input.idempotency_key, b[0].input.idempotency_key,
        "replay must reuse keys"
    );
    assert!(
        a[0].input
            .idempotency_key
            .as_ref()
            .is_some_and(|k| k.contains(":g0:")),
        "keys must embed generation"
    );
}

#[test]
fn payload_objects_reject_unknown_fields_via_lhc_types() {
    let mut bad: Map<String, Value> = Map::new();
    bad.insert("text".into(), json!("ok"));
    bad.insert("extraField".into(), json!(1));
    let err = serde_json::from_value::<TextPayload>(Value::Object(bad)).unwrap_err();
    assert!(
        err.to_string().contains("unknown field") || err.to_string().contains("unknown"),
        "expected deny_unknown_fields, got {err}"
    );
}

/// Chunk 2 — hand-authored serving substitute shape (system prefix + LHC body).
#[test]
fn golden_serving_substitute_preserves_system_prefix() {
    use grok_lhc_host::{ServeDecision, decide_substitution};
    use lhc::shared_tech::view::{
        SessionAssistantMessage, SessionAssistantPart, SessionAssistantPartType, SessionThreadView,
        SessionThreadViewEntry, SessionThreadViewEntrySource, SessionThreadViewMessage,
        SessionUserMessage,
    };
    let native = vec![
        ConversationItem::system("host-system-prefix"),
        ConversationItem::user("stale-native"),
    ];
    let src = |id: &str| SessionThreadViewEntrySource {
        message_id: id.into(),
        idempotency_key: None,
    };
    let view = SessionThreadView {
        thread_id: "golden-serve".into(),
        entries: vec![
            SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
                content: "lhc-user".into(),
                source_messages: vec![src("u")],
            })),
            SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
                SessionAssistantMessage {
                    content: vec![SessionAssistantPart {
                        type_: SessionAssistantPartType::Text,
                        text: Some("lhc-assistant".into()),
                        thinking: None,
                        thinking_signature: None,
                        tool_call_id: None,
                        tool_name: None,
                        arguments: None,
                    }],
                    source_messages: vec![src("a")],

                    provider: None,
                    model: None,
                    api: None,
                },
            )),
        ],
    };
    let kinds = grok_lhc_host::SourceKindIndex::assume_sourced_users_are_prompts(&view);
    let ServeDecision::Substitute { items } = decide_substitution(&native, &view, &kinds, None)
    else {
        panic!("expected substitute");
    };
    let projected: Value = json!(
        items
            .iter()
            .map(|item| match item {
                ConversationItem::System(s) =>
                    json!({ "role": "system", "text": s.content.as_ref() }),
                ConversationItem::User(u) => {
                    let text = u
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(text.as_ref()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    json!({ "role": "user", "text": text })
                }
                ConversationItem::Assistant(a) => {
                    json!({ "role": "assistant", "text": a.content.as_ref() })
                }
                other => panic!("unexpected item in serving golden: {other:?}"),
            })
            .collect::<Vec<_>>()
    );
    let path = goldens_dir().join("serving_substitute.json");
    if std::env::var("UPDATE_GOLDENS").ok().as_deref() == Some("1") {
        fs::write(
            &path,
            serde_json::to_string_pretty(&projected).unwrap() + "\n",
        )
        .unwrap();
    }
    let expected: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("serving_substitute.json"))
            .expect("golden json");
    assert_eq!(
        projected, expected,
        "golden mismatch: serving_substitute.json"
    );
}
