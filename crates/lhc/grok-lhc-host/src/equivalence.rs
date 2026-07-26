//! Hook 4 equivalence instrumentation (Chunk 2 G1).
//!
//! Observes only — never changes the served result. Compares the natively-built
//! request body to the body after LHC serve/substitute.
//!
//! Two signals (do not collapse):
//! - [`structural_divergence`] — raw shape (count, kinds, roles, byte text)
//! - [`informational_divergence`] — after [`project_conversation_canonical`]
//!
//! Armed by default when the caller reaches this path (LHC capture active);
//! set `GROK_LHC_EQUIVALENCE=0` / `false` / `off` to disable.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tracing::warn;
use xai_grok_sampling_types::{
    BackendToolCallItem, BackendToolKind, ContentPart, ConversationItem, rs,
};

/// LHC-aligned tool-call text (`render.rs` literals).
const TOOL_CALL_PREFIX: &str = "[tool call · ";
const TOOL_CALL_CLOSE_SPACE: &str = "] ";
const TOOL_RESULT_PREFIX: &str = "[tool result · ";
const TOOL_RESULT_CLOSE: &str = "]";
const THINKING_OPEN: &str = "[thinking]\n";
const THINKING_CLOSE: &str = "\n[/thinking]";
const UNKNOWN_TOOL: &str = "unknown_tool";

static STRUCTURAL_HITS: AtomicU64 = AtomicU64::new(0);
static INFORMATIONAL_HITS: AtomicU64 = AtomicU64::new(0);
/// Turns where LHC actually substituted and a comparison was recorded.
static SERVE_COMPARED_TURNS: AtomicU64 = AtomicU64::new(0);
/// Turns where serving fell open to native (not equivalence evidence).
static SERVE_FALLBACK_TURNS: AtomicU64 = AtomicU64::new(0);

/// Per-session log classes already emitted (once per session per class).
fn logged_sessions() -> &'static Mutex<HashMap<String, u8>> {
    static LOGGED: OnceLock<Mutex<HashMap<String, u8>>> = OnceLock::new();
    LOGGED.get_or_init(|| Mutex::new(HashMap::new()))
}

const CLASS_STRUCTURAL: u8 = 1;
const CLASS_INFORMATIONAL: u8 = 2;

/// One projected message after canonical text projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedItem {
    pub role: &'static str,
    pub text: String,
}

/// Result of observing native vs served bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquivalenceReport {
    /// Whether this turn was counted as a compared (substituted) turn.
    pub compared: bool,
    /// Whether this turn was a serve fail-open / native fallback.
    pub fallback: bool,
    pub structural_divergence: bool,
    pub informational_divergence: bool,
    pub native_len: usize,
    pub served_len: usize,
    pub native_projected_len: usize,
    pub served_projected_len: usize,
    /// Index of first differing projected item (informational), if any.
    pub first_info_diff_index: Option<usize>,
    /// Index of first differing raw item (structural), if any.
    pub first_struct_diff_index: Option<usize>,
}

/// Self-describing snapshot for Chunk 3 live-cert / removal ruling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EquivalenceSnapshot {
    /// Turns where LHC substituted and native vs served was compared.
    pub turns_served_and_compared: u64,
    /// Turns where serving fell open to native (not evidence).
    pub turns_fallen_back: u64,
    /// Compared turns with raw shape divergence.
    pub structural_divergences: u64,
    /// Compared turns with post-projection content divergence (actionable).
    pub informational_divergences: u64,
}

/// Whether equivalence observation is enabled (default on).
pub fn equivalence_armed() -> bool {
    match std::env::var("GROK_LHC_EQUIVALENCE") {
        Ok(v) => {
            let t = v.trim();
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

/// Line-ending unification applied to both sides before informational compare.
///
/// **Normalizes:** `\r\n` / `\r` → `\n`, and trims a single leading/trailing
/// whitespace run on the whole string (BOM/padding only).
///
/// **Does not normalize:** internal spaces, tabs, indentation, or blank lines.
/// Collapsing Unicode whitespace runs would hide code/JSON payload differences.
pub fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

/// Recursively sort object keys; leave array order intact.
///
/// **Instrument-only** — used solely inside equivalence comparison
/// ([`canonicalize_tool_arguments`] / [`raw_fingerprint`] /
/// [`project_conversation_canonical`]). Never called from serve/write-back
/// translators or capture.
fn sort_json_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map
                .into_iter()
                .map(|(k, v)| (k, sort_json_keys(v)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(entries.into_iter().collect())
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sort_json_keys).collect())
        }
        other => other,
    }
}

/// Parse tool-call arguments and re-serialize compact with **sorted object keys**.
///
/// Native holds provider raw bytes (may be pretty-printed / differently keyed);
/// serving rebuilds from a `serde_json::Map` via `Value::Object(...).to_string()`.
/// Object key order is cosmetic (ruling S2) — sorted here for comparison only.
/// Array element order stays significant. Non-JSON strings pass through
/// unchanged so a genuine parse failure stays visible.
///
/// **Instrument-only** — must not be used when building served or write-back
/// bodies.
pub fn canonicalize_tool_arguments(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => sort_json_keys(v).to_string(),
        Err(_) => raw.to_string(),
    }
}

/// Canonical text projection applied identically to native and served bodies.
///
/// **Normalizes away (only):**
/// - Host item-kind differences that LHC has already flattened to text
///   (`ToolResult` / `Assistant.tool_calls` / `Reasoning` → LHC-shaped text
///   lines using the same prefixes as vendored `render.rs`)
/// - Tool-call **argument formatting** (parse + compact re-serialize with
///   sorted object keys via [`canonicalize_tool_arguments`]) — cosmetic
///   whitespace and object key order must not fire the actionable channel; a
///   real argument value/key change or array reorder still does
/// - `synthetic_reason` / `prompt_index` / model metadata on items (projection
///   is `(role, text)` only)
/// - Line endings / outer trim (via [`normalize_whitespace`])
/// - Contiguous `[context · …` band items collapsed to one (write-back joins
///   N bands into a single `user_meta`; serving emits N items — same
///   information, different representation). Join matches write-back (`\n\n`).
///
/// **Does not normalize away:** ordering, non-band projected item count, or
/// message content (indentation / internal whitespace preserved outside tool
/// argument JSON).
pub fn project_conversation_canonical(items: &[ConversationItem]) -> Vec<ProjectedItem> {
    let mut out = Vec::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for item in items {
        match item {
            ConversationItem::System(s) => out.push(ProjectedItem {
                role: "system",
                text: normalize_whitespace(s.content.as_ref()),
            }),
            ConversationItem::User(u) => {
                let text = u
                    .content
                    .iter()
                    .map(|p| match p {
                        xai_grok_sampling_types::ContentPart::Text { text } => text.as_ref(),
                        xai_grok_sampling_types::ContentPart::Image { .. } => "[image]",
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(ProjectedItem {
                    role: "user",
                    text: normalize_whitespace(&text),
                });
            }
            ConversationItem::Assistant(a) => {
                for tc in &a.tool_calls {
                    tool_names.insert(tc.id.as_ref().to_owned(), tc.name.clone());
                }
                if !a.content.is_empty() {
                    out.push(ProjectedItem {
                        role: "assistant",
                        text: normalize_whitespace(a.content.as_ref()),
                    });
                }
                for tc in &a.tool_calls {
                    let args = canonicalize_tool_arguments(tc.arguments.as_ref());
                    let line =
                        format!("{TOOL_CALL_PREFIX}{}{TOOL_CALL_CLOSE_SPACE}{args}", tc.name,);
                    out.push(ProjectedItem {
                        role: "assistant",
                        text: normalize_whitespace(&line),
                    });
                }
            }
            ConversationItem::ToolResult(tr) => {
                let name = tool_names
                    .get(&tr.tool_call_id)
                    .map(String::as_str)
                    .unwrap_or(UNKNOWN_TOOL);
                let line = format!(
                    "{TOOL_RESULT_PREFIX}{name}{TOOL_RESULT_CLOSE}\n{}",
                    tr.content.as_ref()
                );
                out.push(ProjectedItem {
                    role: "user",
                    text: normalize_whitespace(&line),
                });
            }
            ConversationItem::Reasoning(_) => {
                let line = format!("{THINKING_OPEN}{}{THINKING_CLOSE}", item.text_content());
                out.push(ProjectedItem {
                    role: "assistant",
                    text: normalize_whitespace(&line),
                });
            }
            ConversationItem::BackendToolCall(_) => {
                let line = format!(
                    "{TOOL_CALL_PREFIX}backend{TOOL_CALL_CLOSE_SPACE}{}",
                    item.text_content()
                );
                out.push(ProjectedItem {
                    role: "assistant",
                    text: normalize_whitespace(&line),
                });
            }
        }
    }
    collapse_contiguous_band_projections(out)
}

const BAND_PREFIX: &str = "[context · ";

/// Collapse runs of user-role projected items whose text starts with
/// `[context · ` into one item joined by `\n\n` (write-back band shape).
/// Does **not** collapse non-band items — item-count loss elsewhere remains
/// visible to the informational signal.
fn collapse_contiguous_band_projections(items: Vec<ProjectedItem>) -> Vec<ProjectedItem> {
    let mut out = Vec::with_capacity(items.len());
    let mut band_run: Vec<String> = Vec::new();
    let flush = |out: &mut Vec<ProjectedItem>, band_run: &mut Vec<String>| {
        if band_run.is_empty() {
            return;
        }
        let joined = band_run.join("\n\n");
        band_run.clear();
        out.push(ProjectedItem {
            role: "user",
            text: joined,
        });
    };
    for item in items {
        if item.role == "user" && item.text.starts_with(BAND_PREFIX) {
            band_run.push(item.text);
        } else {
            flush(&mut out, &mut band_run);
            out.push(item);
        }
    }
    flush(&mut out, &mut band_run);
    out
}

/// Length-frame a field so delimiters inside the payload cannot collide
/// across field boundaries (`len:bytes`). Encoding is injective over
/// sequences of string fields.
fn push_framed(out: &mut String, field: &str) {
    out.push('|');
    out.push_str(&field.len().to_string());
    out.push(':');
    out.push_str(field);
}

/// Frame an `Option<&str>`: presence bit `"s"`/`"n"`, then the value.
/// Absent ≠ present-empty (`None` ≠ `Some("")`) — U2.
fn push_option_str(out: &mut String, opt: Option<&str>) {
    match opt {
        None => {
            push_framed(out, "n");
            push_framed(out, "");
        }
        Some(v) => {
            push_framed(out, "s");
            push_framed(out, v);
        }
    }
}

/// Frame an `Option` via `Debug` of the inner value, with a presence bit.
fn push_option_dbg<T: std::fmt::Debug>(out: &mut String, opt: Option<&T>) {
    match opt {
        None => {
            push_framed(out, "n");
            push_framed(out, "");
        }
        Some(v) => {
            push_framed(out, "s");
            push_framed(out, &format!("{v:?}"));
        }
    }
}

/// Structural fingerprint. Length-framing is injective over the **field
/// projections** that are framed (presence bit + value for every `Option`).
/// Every arm must frame typed fields, never a rendered aggregate.
fn raw_fingerprint(item: &ConversationItem) -> String {
    let mut out = String::new();
    match item {
        ConversationItem::System(s) => {
            out.push_str("system");
            push_framed(&mut out, s.content.as_ref());
        }
        ConversationItem::User(u) => {
            out.push_str(if u.synthetic_reason.is_some() {
                "user_meta"
            } else {
                "user"
            });
            // PIN: User.synthetic_reason
            push_option_dbg(&mut out, u.synthetic_reason.as_ref());
            // PIN: User.cwd_generation
            push_option_dbg(&mut out, u.cwd_generation.as_ref());
            // PIN: User.prior_turn_interrupt
            push_option_dbg(&mut out, u.prior_turn_interrupt.as_ref());
            // PIN: User.prompt_index
            push_option_dbg(&mut out, u.prompt_index.as_ref());
            push_framed(&mut out, &u.content.len().to_string());
            for part in &u.content {
                push_content_part(&mut out, part);
            }
        }
        ConversationItem::Assistant(a) if !a.tool_calls.is_empty() => {
            out.push_str("assistant_tools");
            push_assistant_meta(&mut out, a);
            // PIN: AssistantTools.toolcalls_count
            push_framed(&mut out, &a.tool_calls.len().to_string());
            for tc in &a.tool_calls {
                push_framed(&mut out, &tc.name);
                push_framed(&mut out, tc.id.as_ref());
                push_framed(
                    &mut out,
                    &canonicalize_tool_arguments(tc.arguments.as_ref()),
                );
            }
            push_framed(&mut out, a.content.as_ref());
        }
        ConversationItem::Assistant(a) => {
            out.push_str("assistant");
            push_assistant_meta(&mut out, a);
            push_framed(&mut out, a.content.as_ref());
        }
        ConversationItem::ToolResult(tr) => {
            out.push_str("tool_result");
            push_framed(&mut out, &tr.tool_call_id);
            push_framed(&mut out, tr.content.as_ref());
            // PIN: ToolResult.images_count
            push_framed(&mut out, &tr.images.len().to_string());
            for img in &tr.images {
                push_content_part(&mut out, img);
            }
        }
        ConversationItem::BackendToolCall(btc) => {
            out.push_str("backend_tool_call");
            push_backend_tool_fields(&mut out, btc);
        }
        ConversationItem::Reasoning(r) => {
            out.push_str("reasoning");
            push_reasoning_fields(&mut out, r);
        }
    }
    out
}

fn push_content_part(out: &mut String, part: &ContentPart) {
    match part {
        ContentPart::Text { text } => {
            push_framed(out, "t");
            push_framed(out, text.as_ref());
        }
        ContentPart::Image { url } => {
            push_framed(out, "i");
            push_framed(out, url.as_ref());
        }
    }
}

fn push_assistant_meta(out: &mut String, a: &xai_grok_sampling_types::AssistantItem) {
    // PIN: Assistant.model_id
    push_option_str(out, a.model_id.as_deref());
    // PIN: Assistant.model_fingerprint
    push_option_str(out, a.model_fingerprint.as_deref());
    // PIN: Assistant.reasoning_effort
    push_option_dbg(out, a.reasoning_effort.as_ref());
}

/// Frame typed backend fields — never [`BackendToolCallItem::text_summary`].
fn push_backend_tool_fields(out: &mut String, btc: &BackendToolCallItem) {
    match &btc.kind {
        BackendToolKind::WebSearch(ws) => {
            push_framed(out, "web_search");
            push_framed(out, &ws.id);
            push_framed(out, &format!("{:?}", ws.status));
            match &ws.action {
                rs::WebSearchToolCallAction::Search(s) => {
                    push_framed(out, "search");
                    push_framed(out, &s.query);
                    // PIN: WebSearch.sources_count (+ presence; None ≠ Some([]))
                    match &s.sources {
                        None => {
                            push_framed(out, "n");
                            push_framed(out, "0");
                        }
                        Some(sources) => {
                            push_framed(out, "s");
                            push_framed(out, &sources.len().to_string());
                            for src in sources {
                                push_framed(out, &src.r#type);
                                push_framed(out, &src.url);
                            }
                        }
                    }
                }
                rs::WebSearchToolCallAction::OpenPage(o) => {
                    push_framed(out, "open_page");
                    // PIN: OpenPage.url (None ≠ Some(""))
                    push_option_str(out, o.url.as_deref());
                }
                rs::WebSearchToolCallAction::Find(f) => {
                    push_framed(out, "find");
                    push_framed(out, &f.url);
                    push_framed(out, &f.pattern);
                }
                rs::WebSearchToolCallAction::FindInPage(f) => {
                    push_framed(out, "find_in_page");
                    push_framed(out, &f.url);
                    push_framed(out, &f.pattern);
                }
            }
        }
        BackendToolKind::XSearch(ct) => {
            push_framed(out, "x_search");
            push_framed(out, &ct.id);
            // PIN: XSearch.call_id
            push_framed(out, &ct.call_id);
            push_framed(out, &ct.name);
            push_framed(out, &ct.input);
        }
        BackendToolKind::CodeInterpreter(ci) => {
            push_framed(out, "code_interpreter");
            push_framed(out, &ci.id);
            push_framed(out, &format!("{:?}", ci.status));
            // PIN: CodeInterp.container_id
            push_framed(out, &ci.container_id);
            // PIN: CodeInterp.code (None ≠ Some(""))
            push_option_str(out, ci.code.as_deref());
            // outputs: serde_json already distinguishes null vs []
            let outputs = serde_json::to_string(&ci.outputs).unwrap_or_default();
            push_framed(out, &outputs);
        }
    }
}

/// Frame typed reasoning fields — never [`reasoning_item_text`] aggregate.
fn push_reasoning_fields(out: &mut String, r: &rs::ReasoningItem) {
    push_framed(out, &r.id);
    // PIN: Reasoning.status
    push_option_dbg(out, r.status.as_ref());
    // PIN: Reasoning.encrypted (None ≠ Some(""))
    push_option_str(out, r.encrypted_content.as_deref());
    // Count is derived / structurally redundant (see MAPPING.md W3).
    push_framed(out, &r.summary.len().to_string());
    for sp in &r.summary {
        match sp {
            rs::SummaryPart::SummaryText(t) => {
                // Loop terminator for unique decoding after count deletion: this
                // tag must never equal the content presence bits `"n"` / `"s"`
                // that follow the summary loop. If either literal changes, re-check
                // the W3 injectivity argument.
                push_framed(out, "summary_text");
                push_framed(out, &t.text);
            }
        }
    }
    // PIN: Reasoning.content_count (+ presence; None ≠ Some([]))
    match &r.content {
        None => {
            push_framed(out, "n");
            push_framed(out, "0");
        }
        Some(content) => {
            push_framed(out, "s");
            push_framed(out, &content.len().to_string());
            for c in content {
                push_framed(out, &c.text);
            }
        }
    }
}

/// Compare native vs served without side effects (pure).
pub fn compare_serve_equivalence(
    native: &[ConversationItem],
    served: &[ConversationItem],
) -> EquivalenceReport {
    let mut first_struct = None;
    let structural = if native.len() != served.len() {
        first_struct = Some(native.len().min(served.len()));
        true
    } else {
        let mut div = false;
        for (i, (n, s)) in native.iter().zip(served.iter()).enumerate() {
            if raw_fingerprint(n) != raw_fingerprint(s) {
                first_struct = Some(i);
                div = true;
                break;
            }
        }
        div
    };

    let native_p = project_conversation_canonical(native);
    let served_p = project_conversation_canonical(served);
    let mut first_info = None;
    let informational = if native_p.len() != served_p.len() {
        first_info = Some(native_p.len().min(served_p.len()));
        true
    } else {
        let mut div = false;
        for (i, (n, s)) in native_p.iter().zip(served_p.iter()).enumerate() {
            if n != s {
                first_info = Some(i);
                div = true;
                break;
            }
        }
        div
    };

    EquivalenceReport {
        compared: true,
        fallback: false,
        structural_divergence: structural,
        informational_divergence: informational,
        native_len: native.len(),
        served_len: served.len(),
        native_projected_len: native_p.len(),
        served_projected_len: served_p.len(),
        first_info_diff_index: first_info,
        first_struct_diff_index: first_struct,
    }
}

fn empty_report(native_len: usize, served_len: usize, fallback: bool) -> EquivalenceReport {
    EquivalenceReport {
        compared: false,
        fallback,
        structural_divergence: false,
        informational_divergence: false,
        native_len,
        served_len,
        native_projected_len: 0,
        served_projected_len: 0,
        first_info_diff_index: None,
        first_struct_diff_index: None,
    }
}

/// Observe native vs served; log once per session per divergence class.
///
/// **Only substituted turns are equivalence evidence.** When `substituted` is
/// false (fail-open / Native decision), the turn increments
/// `turns_fallen_back` and is **not** compared — identical native==served
/// must not pad the zero-divergence pile.
///
/// Never modifies `served`. Safe to call only when capture is already active
/// (caller gated — zero cost when `GROK_LHC` unset / tee not installed).
pub fn observe_serve_equivalence(
    session_id: &str,
    turn_index: Option<usize>,
    compact_occurred: bool,
    substituted: bool,
    native: &[ConversationItem],
    served: &[ConversationItem],
) -> EquivalenceReport {
    if !equivalence_armed() {
        return empty_report(native.len(), served.len(), !substituted);
    }
    if !substituted {
        SERVE_FALLBACK_TURNS.fetch_add(1, Ordering::Relaxed);
        return empty_report(native.len(), served.len(), true);
    }
    SERVE_COMPARED_TURNS.fetch_add(1, Ordering::Relaxed);
    let report = compare_serve_equivalence(native, served);
    if report.structural_divergence {
        STRUCTURAL_HITS.fetch_add(1, Ordering::Relaxed);
        log_once(LogOnce {
            session_id,
            class: CLASS_STRUCTURAL,
            class_name: "structural",
            turn_index,
            compact_occurred,
            report: &report,
            native,
            served,
        });
    }
    if report.informational_divergence {
        INFORMATIONAL_HITS.fetch_add(1, Ordering::Relaxed);
        log_once(LogOnce {
            session_id,
            class: CLASS_INFORMATIONAL,
            class_name: "informational",
            turn_index,
            compact_occurred,
            report: &report,
            native,
            served,
        });
    }
    report
}

/// Process-wide counters for Chunk 3 live-cert / removal ruling.
pub fn equivalence_snapshot() -> EquivalenceSnapshot {
    EquivalenceSnapshot {
        turns_served_and_compared: SERVE_COMPARED_TURNS.load(Ordering::Relaxed),
        turns_fallen_back: SERVE_FALLBACK_TURNS.load(Ordering::Relaxed),
        structural_divergences: STRUCTURAL_HITS.load(Ordering::Relaxed),
        informational_divergences: INFORMATIONAL_HITS.load(Ordering::Relaxed),
    }
}

struct LogOnce<'a> {
    session_id: &'a str,
    class: u8,
    class_name: &'a str,
    turn_index: Option<usize>,
    compact_occurred: bool,
    report: &'a EquivalenceReport,
    native: &'a [ConversationItem],
    served: &'a [ConversationItem],
}

fn log_once(args: LogOnce<'_>) {
    let mut guard = logged_sessions().lock().unwrap_or_else(|e| e.into_inner());
    let entry = guard.entry(args.session_id.to_string()).or_insert(0);
    if *entry & args.class != 0 {
        return;
    }
    *entry |= args.class;
    let idx = if args.class == CLASS_INFORMATIONAL {
        args.report.first_info_diff_index
    } else {
        args.report.first_struct_diff_index
    };
    let native_sample = idx.and_then(|i| args.native.get(i).map(raw_fingerprint));
    let served_sample = idx.and_then(|i| args.served.get(i).map(raw_fingerprint));
    warn!(
        session_id = args.session_id,
        turn_index = args.turn_index,
        compact_occurred = args.compact_occurred,
        class = args.class_name,
        native_len = args.report.native_len,
        served_len = args.report.served_len,
        native_projected_len = args.report.native_projected_len,
        served_projected_len = args.report.served_projected_len,
        first_diff_index = ?idx,
        native_item = ?native_sample,
        served_item = ?served_sample,
        "LHC hook-4 equivalence divergence (instrumented-redundant; first of class for session)"
    );
}

/// Test/cert helpers.
#[cfg(any(test, feature = "test-util"))]
pub fn structural_hit_count() -> u64 {
    STRUCTURAL_HITS.load(Ordering::Relaxed)
}

#[cfg(any(test, feature = "test-util"))]
pub fn informational_hit_count() -> u64 {
    INFORMATIONAL_HITS.load(Ordering::Relaxed)
}

#[cfg(any(test, feature = "test-util"))]
pub fn serve_compared_turns() -> u64 {
    SERVE_COMPARED_TURNS.load(Ordering::Relaxed)
}

#[cfg(any(test, feature = "test-util"))]
pub fn serve_fallback_turns() -> u64 {
    SERVE_FALLBACK_TURNS.load(Ordering::Relaxed)
}

#[cfg(any(test, feature = "test-util"))]
pub fn reset_equivalence_counters() {
    STRUCTURAL_HITS.store(0, Ordering::Relaxed);
    INFORMATIONAL_HITS.store(0, Ordering::Relaxed);
    SERVE_COMPARED_TURNS.store(0, Ordering::Relaxed);
    SERVE_FALLBACK_TURNS.store(0, Ordering::Relaxed);
    logged_sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::ToolCall;

    #[test]
    fn projection_renders_tools_like_lhc() {
        let items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
                content: "calling".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{\"cmd\":\"ls\"}".into(),
                }],
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
            ConversationItem::tool_result("c1", "file_a"),
        ];
        let p = project_conversation_canonical(&items);
        assert_eq!(p[0].role, "system");
        assert_eq!(p[1].text, "run");
        assert_eq!(p[2].text, "calling");
        assert!(p[3].text.starts_with("[tool call · bash]"));
        assert!(p[4].text.starts_with("[tool result · bash]"));
        assert!(p[4].text.contains("file_a"));
    }

    #[test]
    fn text_only_identical_bodies_no_divergence() {
        let body = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hi"),
            ConversationItem::assistant("hello"),
        ];
        let r = compare_serve_equivalence(&body, &body);
        assert!(!r.structural_divergence);
        assert!(!r.informational_divergence);
    }

    #[test]
    fn projection_preserves_internal_whitespace() {
        let indented = "fn main() {\n    println!(\"hi\");\n}";
        let items = vec![ConversationItem::user(indented)];
        let p = project_conversation_canonical(&items);
        assert_eq!(p[0].text, indented, "indentation must survive projection");
        let collapsed_would_be = "fn main() { println!(\"hi\"); }";
        assert_ne!(p[0].text, collapsed_would_be);
    }

    #[test]
    fn band_collapse_aligns_writeback_and_serving() {
        // Write-back: one CompactionMeta with joined bands.
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user_meta(
                "[context · brief]\nA\n\n[context · detailed]\nB\n\n[context · smooth]\nC",
            ),
            ConversationItem::user("live"),
            ConversationItem::assistant("ok"),
        ];
        // Serving: N separate band items.
        let served = vec![
            ConversationItem::system("sys"),
            ConversationItem::user_meta("[context · brief]\nA"),
            ConversationItem::user_meta("[context · detailed]\nB"),
            ConversationItem::user_meta("[context · smooth]\nC"),
            ConversationItem::user("live"),
            ConversationItem::assistant("ok"),
        ];
        let r = compare_serve_equivalence(&native, &served);
        assert!(
            r.structural_divergence,
            "band representation difference must remain structural"
        );
        assert!(
            !r.informational_divergence,
            "identical band text must not fire informational after collapse — diff at {:?}",
            r.first_info_diff_index
        );
    }

    /// Q3 negative: a **missing** band in serving vs write-back must fire
    /// informational divergence. Would fail if collapse projected a constant.
    #[test]
    fn band_collapse_missing_band_is_informational() {
        let writeback = vec![
            ConversationItem::system("sys"),
            ConversationItem::user_meta(
                "[context · brief]\nA\n\n[context · detailed]\nB\n\n[context · smooth]\nC",
            ),
            ConversationItem::user("live"),
        ];
        // Serving missing the detailed band.
        let served = vec![
            ConversationItem::system("sys"),
            ConversationItem::user_meta("[context · brief]\nA"),
            ConversationItem::user_meta("[context · smooth]\nC"),
            ConversationItem::user("live"),
        ];
        let r = compare_serve_equivalence(&writeback, &served);
        assert!(
            r.informational_divergence,
            "missing band must register informational divergence"
        );
    }

    /// Q3 negative: **reordered** bands must fire informational divergence.
    #[test]
    fn band_collapse_reordered_bands_is_informational() {
        let writeback = vec![
            ConversationItem::system("sys"),
            ConversationItem::user_meta(
                "[context · brief]\nA\n\n[context · detailed]\nB\n\n[context · smooth]\nC",
            ),
            ConversationItem::user("live"),
        ];
        let served = vec![
            ConversationItem::system("sys"),
            ConversationItem::user_meta("[context · detailed]\nB"),
            ConversationItem::user_meta("[context · brief]\nA"),
            ConversationItem::user_meta("[context · smooth]\nC"),
            ConversationItem::user("live"),
        ];
        let r = compare_serve_equivalence(&writeback, &served);
        assert!(
            r.informational_divergence,
            "reordered bands must register informational divergence"
        );
    }

    fn assistant_tool(name: &str, id: &str, args: &str) -> ConversationItem {
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    fn assistant_tool_with_content(
        name: &str,
        id: &str,
        args: &str,
        content: &str,
    ) -> ConversationItem {
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: content.into(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    /// Serve side via real translator: `SessionThreadView` →
    /// [`crate::serving::session_view_to_serve_items`] /
    /// `emit_assistant_conserved`.
    fn served_via_translator(args_json: &str, tool_name: &str) -> Vec<ConversationItem> {
        use crate::serving::{SourceKindIndex, session_view_to_serve_items};
        use lhc::shared_tech::view::{
            SessionAssistantMessage, SessionAssistantPart, SessionAssistantPartType,
            SessionThreadView, SessionThreadViewEntry, SessionThreadViewEntrySource,
            SessionThreadViewMessage, SessionUserMessage,
        };
        let args: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(args_json).expect("args json");
        let view = SessionThreadView {
            thread_id: "t".into(),
            entries: vec![
                SessionThreadViewEntry::Message(SessionThreadViewMessage::User(
                    SessionUserMessage {
                        content: "run".into(),
                        source_messages: vec![SessionThreadViewEntrySource {
                            message_id: "u".into(),
                            idempotency_key: Some("u".into()),
                        }],
                    },
                )),
                SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
                    SessionAssistantMessage {
                        content: vec![SessionAssistantPart {
                            type_: SessionAssistantPartType::ToolCall,
                            text: None,
                            thinking: None,
                            tool_call_id: Some("c1".into()),
                            tool_name: Some(tool_name.into()),
                            arguments: Some(args),
                        }],
                        source_messages: vec![SessionThreadViewEntrySource {
                            message_id: "a".into(),
                            idempotency_key: Some("a".into()),
                        }],
                    },
                )),
            ],
        };
        let kinds = SourceKindIndex::assume_sourced_users_are_prompts(&view);
        let mut items = vec![ConversationItem::system("sys")];
        items.extend(session_view_to_serve_items(&view, &kinds).expect("translate"));
        items
    }

    /// S1 — delimiter collision under the old `name@id:args:content` scheme
    /// must produce distinct fingerprints with length framing.
    #[test]
    fn raw_fingerprint_assistant_tools_is_injective_across_delimiter_collision() {
        // Old scheme: both → `assistant_tools:n@i:x:y:z`
        let a = assistant_tool_with_content("n", "i", "x:y", "z");
        let b = assistant_tool_with_content("n", "i", "x", "y:z");
        assert_ne!(
            raw_fingerprint(&a),
            raw_fingerprint(&b),
            "args/content delimiter collision must not share a fingerprint"
        );
        let r = compare_serve_equivalence(
            &[ConversationItem::system("s"), a],
            &[ConversationItem::system("s"), b],
        );
        assert!(
            r.structural_divergence,
            "injectivity failure would under-report structural divergence"
        );
    }

    /// S1 — `tool_result:{id}:{content}` collision pair.
    #[test]
    fn raw_fingerprint_tool_result_is_injective_across_delimiter_collision() {
        // Old scheme: both → `tool_result:c1:x:y`
        let a = ConversationItem::tool_result("c1", "x:y");
        let b = ConversationItem::tool_result("c1:x", "y");
        assert_ne!(
            raw_fingerprint(&a),
            raw_fingerprint(&b),
            "tool_call_id/content delimiter collision must not share a fingerprint"
        );
        let r = compare_serve_equivalence(&[a], &[b]);
        assert!(r.structural_divergence);
    }

    #[test]
    fn canonicalize_tool_arguments_collapses_cosmetic_json() {
        const PRETTY: &str = "{\n  \"cmd\": \"ls\",\n  \"timeout_ms\": 5000\n}";
        const COMPACT: &str = r#"{"cmd":"ls","timeout_ms":5000}"#;
        assert_eq!(
            canonicalize_tool_arguments(PRETTY),
            canonicalize_tool_arguments(COMPACT)
        );
        assert_ne!(
            canonicalize_tool_arguments(r#"{"cmd":"ls"}"#),
            canonicalize_tool_arguments(r#"{"cmd":"pwd"}"#)
        );
    }

    /// S2 — object key reorder is cosmetic; must be silent on both channels.
    #[test]
    fn tool_arg_object_key_reorder_is_silent() {
        const AB: &str = r#"{"a":1,"b":2}"#;
        const BA: &str = r#"{"b":2,"a":1}"#;
        assert_ne!(AB, BA, "fixture invalid: raw key order must differ");
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            assistant_tool("bash", "c1", AB),
        ];
        let served = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            assistant_tool("bash", "c1", BA),
        ];
        let r = compare_serve_equivalence(&native, &served);
        assert!(
            !r.structural_divergence,
            "key reorder must not fire structural"
        );
        assert!(
            !r.informational_divergence,
            "key reorder must not fire informational"
        );
    }

    /// S2 — array element order is significant.
    #[test]
    fn tool_arg_array_reorder_is_divergent() {
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            assistant_tool("bash", "c1", r#"{"xs":[1,2]}"#),
        ];
        let served = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            assistant_tool("bash", "c1", r#"{"xs":[2,1]}"#),
        ];
        let r = compare_serve_equivalence(&native, &served);
        assert!(
            r.informational_divergence,
            "array reorder must register informational"
        );
        assert!(r.structural_divergence);
    }

    /// S3 — native provider-raw vs **real translator** serve path.
    /// Cosmetic pretty-vs-compact must be silent. Would fail if projection
    /// compared raw argument bytes from emit_assistant_conserved.
    #[test]
    fn tool_arg_cosmetic_via_translator_is_silent() {
        const PRETTY: &str = "{\n  \"cmd\": \"ls\",\n  \"timeout_ms\": 5000\n}";
        const COMPACT: &str = r#"{"cmd":"ls","timeout_ms":5000}"#;
        assert_ne!(PRETTY, COMPACT);
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            assistant_tool("bash", "c1", PRETTY),
        ];
        let served = served_via_translator(COMPACT, "bash");
        // Prove served actually came from translator (compact args on item).
        match served.last() {
            Some(ConversationItem::Assistant(a)) => {
                assert_eq!(a.tool_calls[0].arguments.as_ref(), COMPACT);
            }
            other => panic!("expected translated Assistant, got {other:?}"),
        }
        let r = compare_serve_equivalence(&native, &served);
        assert!(!r.structural_divergence);
        assert!(
            !r.informational_divergence,
            "translator cosmetic path must be silent — diff at {:?}",
            r.first_info_diff_index
        );
    }

    /// S3 — real arg change: native vs translator-built serve body.
    #[test]
    fn tool_arg_real_change_via_translator_is_informational() {
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            assistant_tool("bash", "c1", r#"{"cmd":"ls"}"#),
        ];
        let served = served_via_translator(r#"{"cmd":"pwd"}"#, "bash");
        let r = compare_serve_equivalence(&native, &served);
        assert!(r.informational_divergence);
        assert!(r.structural_divergence);
    }

    /// S3 — swapped tool name through translator registers structurally.
    #[test]
    fn swapped_tool_call_via_translator_registers_structurally() {
        let native = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("run"),
            assistant_tool("bash", "c1", r#"{"cmd":"ls"}"#),
        ];
        let served = served_via_translator(r#"{"cmd":"ls"}"#, "python");
        let r = compare_serve_equivalence(&native, &served);
        assert!(
            r.structural_divergence,
            "swapped tool name via translator must register structurally"
        );
    }

    fn web_search(id: &str, query: &str, status: rs::WebSearchToolCallStatus) -> ConversationItem {
        ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: id.into(),
                status,
                action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                    query: query.into(),
                    sources: Some(vec![]),
                }),
            }),
        })
    }

    fn web_search_with_sources(
        id: &str,
        query: &str,
        sources: Vec<rs::WebSearchActionSearchSource>,
    ) -> ConversationItem {
        ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: id.into(),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                    query: query.into(),
                    sources: Some(sources),
                }),
            }),
        })
    }

    /// T1 — same query, different backend ids must not collide (old scheme
    /// framed `text_summary()` which omitted id).
    #[test]
    fn raw_fingerprint_backend_tool_distinct_ids_do_not_collide() {
        let a = web_search("ws_a", "same query", rs::WebSearchToolCallStatus::Completed);
        let b = web_search("ws_b", "same query", rs::WebSearchToolCallStatus::Completed);
        assert_eq!(
            a.text_content(),
            b.text_content(),
            "fixture: same aggregate"
        );
        assert_ne!(
            raw_fingerprint(&a),
            raw_fingerprint(&b),
            "backend call id must be framed"
        );
        assert!(compare_serve_equivalence(&[a], &[b]).structural_divergence);
    }

    /// T1 — status-only difference registers.
    #[test]
    fn raw_fingerprint_backend_tool_status_difference_registers() {
        let a = web_search("ws_1", "q", rs::WebSearchToolCallStatus::Completed);
        let b = web_search("ws_1", "q", rs::WebSearchToolCallStatus::Failed);
        assert_ne!(raw_fingerprint(&a), raw_fingerprint(&b));
        assert!(compare_serve_equivalence(&[a], &[b]).structural_divergence);
    }

    /// T1 — sources-only difference registers (count framed before entries).
    #[test]
    fn raw_fingerprint_backend_tool_sources_difference_registers() {
        let a = web_search_with_sources("ws_1", "q", vec![]);
        let b = web_search_with_sources(
            "ws_1",
            "q",
            vec![rs::WebSearchActionSearchSource {
                r#type: "url".into(),
                url: "https://example.com".into(),
            }],
        );
        assert_eq!(
            a.text_content(),
            b.text_content(),
            "fixture: same aggregate"
        );
        assert_ne!(raw_fingerprint(&a), raw_fingerprint(&b));
        assert!(compare_serve_equivalence(&[a], &[b]).structural_divergence);
    }

    /// T1/W2 — summary part boundaries only (same id, same count). Would
    /// pass on `summary_count` alone if count differed (2 vs 1); equal
    /// counts force the loop body to be the distinguisher.
    #[test]
    fn raw_fingerprint_reasoning_summary_parts_do_not_collide() {
        let a = ConversationItem::Reasoning(rs::ReasoningItem {
            id: "r".into(),
            summary: vec![
                rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "a\nb".into(),
                }),
                rs::SummaryPart::SummaryText(rs::SummaryTextContent { text: "c".into() }),
            ],
            content: None,
            encrypted_content: None,
            status: None,
        });
        let b = ConversationItem::Reasoning(rs::ReasoningItem {
            id: "r".into(),
            summary: vec![
                rs::SummaryPart::SummaryText(rs::SummaryTextContent { text: "a".into() }),
                rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "b\nc".into(),
                }),
            ],
            content: None,
            encrypted_content: None,
            status: None,
        });
        let (ConversationItem::Reasoning(ra), ConversationItem::Reasoning(rb)) = (&a, &b) else {
            unreachable!()
        };
        assert_eq!(ra.summary.len(), rb.summary.len(), "fixture: same count");
        assert_eq!(
            a.text_content(),
            b.text_content(),
            "fixture: same aggregate"
        );
        assert_ne!(
            raw_fingerprint(&a),
            raw_fingerprint(&b),
            "summary part boundaries must be framed"
        );
        assert!(compare_serve_equivalence(&[a], &[b]).structural_divergence);
    }

    /// T1 — encrypted-only reasoning with different payloads must differ.
    #[test]
    fn raw_fingerprint_reasoning_encrypted_only_differs() {
        let a = ConversationItem::Reasoning(rs::ReasoningItem {
            id: String::new(),
            summary: vec![],
            content: None,
            encrypted_content: Some("enc_aaa".into()),
            status: None,
        });
        let b = ConversationItem::Reasoning(rs::ReasoningItem {
            id: String::new(),
            summary: vec![],
            content: None,
            encrypted_content: Some("enc_bbb".into()),
            status: None,
        });
        assert_eq!(a.text_content(), b.text_content());
        assert_ne!(
            raw_fingerprint(&a),
            raw_fingerprint(&b),
            "encrypted_content must be framed"
        );
        assert!(compare_serve_equivalence(&[a], &[b]).structural_divergence);
    }

    // ── U1/U2: one pin per framing field (differ only in that field) ──────

    fn assert_fp_diff(a: ConversationItem, b: ConversationItem, field: &str) {
        assert_ne!(
            raw_fingerprint(&a),
            raw_fingerprint(&b),
            "PIN {field}: fingerprints must differ when only that field differs"
        );
    }

    fn base_user() -> xai_grok_sampling_types::UserItem {
        xai_grok_sampling_types::UserItem {
            content: vec![ContentPart::Text { text: "hi".into() }],
            synthetic_reason: None,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        }
    }

    #[test]
    fn pin_user_synthetic_reason() {
        use xai_grok_sampling_types::SyntheticReason;
        let mut a = base_user();
        let mut b = base_user();
        a.synthetic_reason = Some(SyntheticReason::CompactionMeta);
        b.synthetic_reason = Some(SyntheticReason::SystemReminder);
        assert_fp_diff(
            ConversationItem::User(a),
            ConversationItem::User(b),
            "User.synthetic_reason",
        );
    }

    #[test]
    fn pin_user_cwd_generation() {
        let mut a = base_user();
        let mut b = base_user();
        a.cwd_generation = Some(1);
        b.cwd_generation = Some(2);
        assert_fp_diff(
            ConversationItem::User(a),
            ConversationItem::User(b),
            "User.cwd_generation",
        );
    }

    #[test]
    fn pin_user_prior_turn_interrupt() {
        use xai_grok_sampling_types::PriorTurnInterrupt;
        let mut a = base_user();
        let mut b = base_user();
        a.prior_turn_interrupt = Some(PriorTurnInterrupt::MidTurnAbort);
        b.prior_turn_interrupt = Some(PriorTurnInterrupt::PermissionRejected);
        assert_fp_diff(
            ConversationItem::User(a),
            ConversationItem::User(b),
            "User.prior_turn_interrupt",
        );
    }

    #[test]
    fn pin_user_prompt_index() {
        let mut a = base_user();
        let mut b = base_user();
        a.prompt_index = Some(0);
        b.prompt_index = Some(1);
        assert_fp_diff(
            ConversationItem::User(a),
            ConversationItem::User(b),
            "User.prompt_index",
        );
    }

    fn base_assistant() -> xai_grok_sampling_types::AssistantItem {
        xai_grok_sampling_types::AssistantItem {
            content: "ok".into(),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn pin_assistant_model_id() {
        let mut a = base_assistant();
        let mut b = base_assistant();
        a.model_id = Some("m1".into());
        b.model_id = Some("m2".into());
        assert_fp_diff(
            ConversationItem::Assistant(a),
            ConversationItem::Assistant(b),
            "Assistant.model_id",
        );
    }

    #[test]
    fn pin_assistant_model_fingerprint() {
        let mut a = base_assistant();
        let mut b = base_assistant();
        a.model_fingerprint = Some("fp1".into());
        b.model_fingerprint = Some("fp2".into());
        assert_fp_diff(
            ConversationItem::Assistant(a),
            ConversationItem::Assistant(b),
            "Assistant.model_fingerprint",
        );
    }

    #[test]
    fn pin_assistant_reasoning_effort() {
        use xai_grok_sampling_types::ReasoningEffort;
        let mut a = base_assistant();
        let mut b = base_assistant();
        a.reasoning_effort = Some(ReasoningEffort::Low);
        b.reasoning_effort = Some(ReasoningEffort::High);
        assert_fp_diff(
            ConversationItem::Assistant(a),
            ConversationItem::Assistant(b),
            "Assistant.reasoning_effort",
        );
    }

    fn assistant_tools_one(name: &str, id: &str, args: &str, content: &str) -> ConversationItem {
        ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
            content: content.into(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.into(),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    #[test]
    fn pin_system_content() {
        assert_fp_diff(
            ConversationItem::system("a"),
            ConversationItem::system("b"),
            "System.content",
        );
    }

    #[test]
    fn pin_content_part_text_tag() {
        // Payload must not equal `"t"` — that would emit `|1:t|` and confound.
        let item = ConversationItem::User(xai_grok_sampling_types::UserItem {
            content: vec![ContentPart::Text {
                text: "payload".into(),
            }],
            ..base_user()
        });
        assert!(
            raw_fingerprint(&item).contains("|1:t|"),
            "ContentPart.Text tag must be framed"
        );
    }

    #[test]
    fn pin_content_part_image_tag() {
        // URL must not equal `"i"` — that would emit `|1:i|` and confound.
        let item = ConversationItem::ToolResult(xai_grok_sampling_types::ToolResultItem {
            tool_call_id: "c1".into(),
            content: "out".into(),
            images: vec![ContentPart::Image {
                url: "data:image/png;base64,AA".into(),
            }],
        });
        assert!(
            raw_fingerprint(&item).contains("|1:i|"),
            "ContentPart.Image tag must be framed"
        );
    }

    #[test]
    fn pin_content_part_text_payload() {
        let mut a = base_user();
        let mut b = base_user();
        a.content = vec![ContentPart::Text { text: "a".into() }];
        b.content = vec![ContentPart::Text { text: "b".into() }];
        assert_fp_diff(
            ConversationItem::User(a),
            ConversationItem::User(b),
            "ContentPart.Text.text",
        );
    }

    #[test]
    fn pin_content_part_image_url() {
        let a = ConversationItem::ToolResult(xai_grok_sampling_types::ToolResultItem {
            tool_call_id: "c1".into(),
            content: "out".into(),
            images: vec![ContentPart::Image {
                url: "data:a".into(),
            }],
        });
        let b = ConversationItem::ToolResult(xai_grok_sampling_types::ToolResultItem {
            tool_call_id: "c1".into(),
            content: "out".into(),
            images: vec![ContentPart::Image {
                url: "data:b".into(),
            }],
        });
        assert_fp_diff(a, b, "ContentPart.Image.url");
    }

    #[test]
    fn pin_assistant_tools_tc_name() {
        assert_fp_diff(
            assistant_tools_one("bash", "c1", "{}", ""),
            assistant_tools_one("python", "c1", "{}", ""),
            "AsstTools.tc.name",
        );
    }

    #[test]
    fn pin_assistant_tools_tc_id() {
        assert_fp_diff(
            assistant_tools_one("bash", "c1", "{}", ""),
            assistant_tools_one("bash", "c2", "{}", ""),
            "AsstTools.tc.id",
        );
    }

    #[test]
    fn pin_assistant_tools_tc_arguments() {
        assert_fp_diff(
            assistant_tools_one("bash", "c1", r#"{"cmd":"ls"}"#, ""),
            assistant_tools_one("bash", "c1", r#"{"cmd":"pwd"}"#, ""),
            "AsstTools.tc.arguments",
        );
    }

    #[test]
    fn pin_assistant_tools_content() {
        assert_fp_diff(
            assistant_tools_one("bash", "c1", "{}", "a"),
            assistant_tools_one("bash", "c1", "{}", "b"),
            "AsstTools.content",
        );
    }

    #[test]
    fn pin_assistant_content() {
        let mut a = base_assistant();
        let mut b = base_assistant();
        a.content = "a".into();
        b.content = "b".into();
        assert_fp_diff(
            ConversationItem::Assistant(a),
            ConversationItem::Assistant(b),
            "Assistant.content",
        );
    }

    #[test]
    fn pin_tool_result_tool_call_id() {
        assert_fp_diff(
            ConversationItem::tool_result("c1", "out"),
            ConversationItem::tool_result("c2", "out"),
            "ToolResult.tool_call_id",
        );
    }

    #[test]
    fn pin_tool_result_content() {
        assert_fp_diff(
            ConversationItem::tool_result("c1", "a"),
            ConversationItem::tool_result("c1", "b"),
            "ToolResult.content",
        );
    }

    #[test]
    fn pin_xsearch_call_id() {
        let mk = |call_id: &str| {
            let ct: rs::CustomToolCall = serde_json::from_value(serde_json::json!({
                "id": "xs_1",
                "call_id": call_id,
                "name": "x_search",
                "input": "q",
            }))
            .unwrap();
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::XSearch(ct),
            })
        };
        assert_fp_diff(mk("call_a"), mk("call_b"), "XSearch.call_id");
    }

    #[test]
    fn pin_xsearch_id() {
        let mk = |id: &str| {
            let ct: rs::CustomToolCall = serde_json::from_value(serde_json::json!({
                "id": id,
                "call_id": "call",
                "name": "x_search",
                "input": "q",
            }))
            .unwrap();
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::XSearch(ct),
            })
        };
        assert_fp_diff(mk("xs_a"), mk("xs_b"), "XSearch.id");
    }

    #[test]
    fn pin_xsearch_name() {
        let mk = |name: &str| {
            let ct: rs::CustomToolCall = serde_json::from_value(serde_json::json!({
                "id": "xs_1",
                "call_id": "call",
                "name": name,
                "input": "q",
            }))
            .unwrap();
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::XSearch(ct),
            })
        };
        assert_fp_diff(mk("x_search"), mk("other"), "XSearch.name");
    }

    #[test]
    fn pin_xsearch_input() {
        let mk = |input: &str| {
            let ct: rs::CustomToolCall = serde_json::from_value(serde_json::json!({
                "id": "xs_1",
                "call_id": "call",
                "name": "x_search",
                "input": input,
            }))
            .unwrap();
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::XSearch(ct),
            })
        };
        assert_fp_diff(mk("q1"), mk("q2"), "XSearch.input");
    }

    fn code_interp(
        id: &str,
        status: rs::CodeInterpreterToolCallStatus,
        container_id: &str,
        code: Option<&str>,
    ) -> ConversationItem {
        ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::CodeInterpreter(rs::CodeInterpreterToolCall {
                id: id.into(),
                container_id: container_id.into(),
                code: code.map(str::to_string),
                outputs: None,
                status,
            }),
        })
    }

    #[test]
    fn pin_codeinterp_container_id() {
        assert_fp_diff(
            code_interp(
                "ci_1",
                rs::CodeInterpreterToolCallStatus::Completed,
                "ctr_a",
                Some("print(1)"),
            ),
            code_interp(
                "ci_1",
                rs::CodeInterpreterToolCallStatus::Completed,
                "ctr_b",
                Some("print(1)"),
            ),
            "CodeInterp.container_id",
        );
    }

    #[test]
    fn pin_codeinterp_id() {
        assert_fp_diff(
            code_interp(
                "ci_a",
                rs::CodeInterpreterToolCallStatus::Completed,
                "ctr",
                Some("print(1)"),
            ),
            code_interp(
                "ci_b",
                rs::CodeInterpreterToolCallStatus::Completed,
                "ctr",
                Some("print(1)"),
            ),
            "CodeInterp.id",
        );
    }

    #[test]
    fn pin_codeinterp_status() {
        assert_fp_diff(
            code_interp(
                "ci_1",
                rs::CodeInterpreterToolCallStatus::Completed,
                "ctr",
                Some("print(1)"),
            ),
            code_interp(
                "ci_1",
                rs::CodeInterpreterToolCallStatus::Failed,
                "ctr",
                Some("print(1)"),
            ),
            "CodeInterp.status",
        );
    }

    #[test]
    fn pin_codeinterp_code_value() {
        assert_fp_diff(
            code_interp(
                "ci_1",
                rs::CodeInterpreterToolCallStatus::Completed,
                "ctr",
                Some("a"),
            ),
            code_interp(
                "ci_1",
                rs::CodeInterpreterToolCallStatus::Completed,
                "ctr",
                Some("b"),
            ),
            "CodeInterp.code value",
        );
    }

    #[test]
    fn pin_reasoning_status() {
        let mk = |status: Option<rs::OutputStatus>| {
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r".into(),
                summary: vec![],
                content: None,
                encrypted_content: None,
                status,
            })
        };
        assert_fp_diff(
            mk(Some(rs::OutputStatus::Completed)),
            mk(Some(rs::OutputStatus::Incomplete)),
            "Reasoning.status",
        );
    }

    #[test]
    fn pin_reasoning_id() {
        let mk = |id: &str| {
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: id.into(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "s".into(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            })
        };
        assert_fp_diff(mk("r1"), mk("r2"), "Reasoning.id");
    }

    #[test]
    fn pin_reasoning_summary_text() {
        let mk = |text: &str| {
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r".into(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: text.into(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            })
        };
        assert_fp_diff(mk("a"), mk("b"), "Reasoning.summary.text");
    }

    #[test]
    fn pin_reasoning_summary_tag() {
        // Text must not equal `"summary_text"` — that would confound `.contains`.
        let item = ConversationItem::Reasoning(rs::ReasoningItem {
            id: "r".into(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: "part".into(),
            })],
            content: None,
            encrypted_content: None,
            status: None,
        });
        assert!(
            raw_fingerprint(&item).contains("|12:summary_text|"),
            "summary_text tag must be framed"
        );
    }

    #[test]
    fn pin_reasoning_content_text() {
        let mk = |text: &str| {
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r".into(),
                summary: vec![],
                content: Some(vec![rs::ReasoningTextContent { text: text.into() }]),
                encrypted_content: None,
                status: None,
            })
        };
        assert_fp_diff(mk("a"), mk("b"), "Reasoning.content.text");
    }

    fn find_action(url: &str, pattern: &str) -> ConversationItem {
        ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: "ws_1".into(),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::Find(rs::WebSearchActionFind {
                    url: url.into(),
                    pattern: pattern.into(),
                }),
            }),
        })
    }

    fn find_in_page_action(url: &str, pattern: &str) -> ConversationItem {
        ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: "ws_1".into(),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::FindInPage(rs::WebSearchActionFind {
                    url: url.into(),
                    pattern: pattern.into(),
                }),
            }),
        })
    }

    /// W2 — Find.url only (same pattern, same variant).
    #[test]
    fn pin_websearch_find_url() {
        assert_fp_diff(
            find_action("https://a.example", "p"),
            find_action("https://b.example", "p"),
            "WS.Find.url",
        );
    }

    /// W2 — Find.pattern only.
    #[test]
    fn pin_websearch_find_pattern() {
        assert_fp_diff(
            find_action("https://example.com", "a"),
            find_action("https://example.com", "b"),
            "WS.Find.pattern",
        );
    }

    #[test]
    fn pin_websearch_find_in_page_url() {
        assert_fp_diff(
            find_in_page_action("https://a.example", "p"),
            find_in_page_action("https://b.example", "p"),
            "WS.FindInPage.url",
        );
    }

    #[test]
    fn pin_websearch_find_in_page_pattern() {
        assert_fp_diff(
            find_in_page_action("https://example.com", "a"),
            find_in_page_action("https://example.com", "b"),
            "WS.FindInPage.pattern",
        );
    }

    #[test]
    fn pin_websearch_search_query() {
        assert_fp_diff(
            web_search("ws_1", "q1", rs::WebSearchToolCallStatus::Completed),
            web_search("ws_1", "q2", rs::WebSearchToolCallStatus::Completed),
            "WS.Search.query",
        );
    }

    #[test]
    fn pin_websearch_source_type() {
        let a = web_search_with_sources(
            "ws_1",
            "q",
            vec![rs::WebSearchActionSearchSource {
                r#type: "url".into(),
                url: "https://a.example".into(),
            }],
        );
        let b = web_search_with_sources(
            "ws_1",
            "q",
            vec![rs::WebSearchActionSearchSource {
                r#type: "api".into(),
                url: "https://a.example".into(),
            }],
        );
        assert_fp_diff(a, b, "WS.Search.src.type");
    }

    #[test]
    fn pin_websearch_source_url() {
        let a = web_search_with_sources(
            "ws_1",
            "q",
            vec![rs::WebSearchActionSearchSource {
                r#type: "url".into(),
                url: "https://a.example".into(),
            }],
        );
        let b = web_search_with_sources(
            "ws_1",
            "q",
            vec![rs::WebSearchActionSearchSource {
                r#type: "url".into(),
                url: "https://b.example".into(),
            }],
        );
        assert_fp_diff(a, b, "WS.Search.src.url");
    }

    #[test]
    fn pin_websearch_open_page_url_value() {
        let mk = |url: &str| {
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                    id: "ws_1".into(),
                    status: rs::WebSearchToolCallStatus::Completed,
                    action: rs::WebSearchToolCallAction::OpenPage(rs::WebSearchActionOpenPage {
                        url: Some(url.into()),
                    }),
                }),
            })
        };
        assert_fp_diff(
            mk("https://a.example"),
            mk("https://b.example"),
            "OpenPage.url",
        );
    }

    /// Framed discriminant tag pin. Fixture fields must not equal `literal` —
    /// `.contains("|len:literal|")` would otherwise pass on a payload field alone (X1).
    fn assert_tag_framed(item: &ConversationItem, framed: &str, field: &str) {
        assert!(
            raw_fingerprint(item).contains(framed),
            "PIN {field}: fingerprint must contain {framed}"
        );
    }

    #[test]
    fn pin_tag_web_search() {
        // id/query must not equal `"web_search"`.
        assert_tag_framed(
            &web_search("ws_1", "qry", rs::WebSearchToolCallStatus::Completed),
            "|10:web_search|",
            "WS.kind_tag",
        );
    }

    #[test]
    fn pin_tag_search_action() {
        // query must not equal `"search"` (would emit `|6:search|` itself).
        assert_tag_framed(
            &web_search("ws_1", "qry", rs::WebSearchToolCallStatus::Completed),
            "|6:search|",
            "WS.Search.action_tag",
        );
    }

    #[test]
    fn pin_tag_open_page_action() {
        let open = ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: "ws_1".into(),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::OpenPage(rs::WebSearchActionOpenPage {
                    url: Some("https://example.com".into()),
                }),
            }),
        });
        assert_tag_framed(&open, "|9:open_page|", "WS.OpenPage.action_tag");
    }

    #[test]
    fn pin_tag_find_action() {
        // url/pattern must not equal `"find"`.
        assert_tag_framed(
            &find_action("https://example.com/path", "pat"),
            "|4:find|",
            "WS.Find.action_tag",
        );
    }

    #[test]
    fn pin_tag_find_in_page_action() {
        // url/pattern must not equal `"find_in_page"`.
        assert_tag_framed(
            &find_in_page_action("https://example.com/path", "pat"),
            "|12:find_in_page|",
            "WS.FindInPage.action_tag",
        );
    }

    #[test]
    fn pin_tag_x_search() {
        // X1: `name` must not equal the kind tag `"x_search"` — that alone
        // would satisfy `.contains("|8:x_search|")` with the kind tag deleted.
        let ct: rs::CustomToolCall = serde_json::from_value(serde_json::json!({
            "id": "xs_1",
            "call_id": "call",
            "name": "nm",
            "input": "qry",
        }))
        .unwrap();
        let item = ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::XSearch(ct),
        });
        assert_tag_framed(&item, "|8:x_search|", "XSearch.kind_tag");
    }

    #[test]
    fn pin_tag_code_interpreter() {
        // id/code/container must not equal `"code_interpreter"`.
        assert_tag_framed(
            &code_interp(
                "ci_1",
                rs::CodeInterpreterToolCallStatus::Completed,
                "ctr",
                Some("print(1)"),
            ),
            "|16:code_interpreter|",
            "CodeInterp.kind_tag",
        );
    }

    // ── X3: ConversationItem kind prefixes (`out.push_str`, unframed) ────

    fn assert_kind_prefix(item: &ConversationItem, prefix: &str, field: &str) {
        let fp = raw_fingerprint(item);
        assert!(
            fp.starts_with(prefix),
            "PIN {field}: fingerprint must start with {prefix:?}, got {:?}",
            &fp[..fp.len().min(32)]
        );
    }

    #[test]
    fn pin_kind_system() {
        assert_kind_prefix(&ConversationItem::system("hi"), "system", "kind.system");
    }

    #[test]
    fn pin_kind_user() {
        assert_kind_prefix(&ConversationItem::User(base_user()), "user|", "kind.user");
    }

    #[test]
    fn pin_kind_user_meta() {
        use xai_grok_sampling_types::SyntheticReason;
        let mut u = base_user();
        u.synthetic_reason = Some(SyntheticReason::CompactionMeta);
        assert_kind_prefix(&ConversationItem::User(u), "user_meta", "kind.user_meta");
    }

    #[test]
    fn pin_kind_assistant_tools() {
        assert_kind_prefix(
            &assistant_tools_one("bash", "c1", "{}", ""),
            "assistant_tools",
            "kind.assistant_tools",
        );
    }

    #[test]
    fn pin_kind_assistant() {
        // Must not use starts_with("assistant") — that matches assistant_tools.
        assert_kind_prefix(
            &ConversationItem::Assistant(base_assistant()),
            "assistant|",
            "kind.assistant",
        );
    }

    #[test]
    fn pin_kind_tool_result() {
        assert_kind_prefix(
            &ConversationItem::tool_result("c1", "out"),
            "tool_result",
            "kind.tool_result",
        );
    }

    #[test]
    fn pin_kind_backend_tool_call() {
        assert_kind_prefix(
            &web_search("ws_1", "qry", rs::WebSearchToolCallStatus::Completed),
            "backend_tool_call",
            "kind.backend_tool_call",
        );
    }

    #[test]
    fn pin_kind_reasoning() {
        assert_kind_prefix(
            &ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r".into(),
                summary: vec![],
                content: None,
                encrypted_content: None,
                status: None,
            }),
            "reasoning",
            "kind.reasoning",
        );
    }

    // ── X2: jointly pinned presence pairs (delete both arms in BWR) ──────

    /// Joint pin: `WS.sources` presence `"n"` (None) + `"s"` (Some).
    /// Deleting either arm alone leaves None vs Some([]) distinguishable by
    /// field-count asymmetry; deleting both collapses them.
    #[test]
    fn pin_joint_websearch_sources_presence_pair() {
        let absent = ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: "ws_1".into(),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                    query: "qry".into(),
                    sources: None,
                }),
            }),
        });
        let empty = web_search_with_sources("ws_1", "qry", vec![]);
        assert_ne!(
            raw_fingerprint(&absent),
            raw_fingerprint(&empty),
            "joint PIN WS.sources presence pair (n+s): None ≠ Some([])"
        );
    }

    /// Joint pin: `Reasoning.content` presence `"n"` (None) + `"s"` (Some).
    #[test]
    fn pin_joint_reasoning_content_presence_pair() {
        let absent = ConversationItem::Reasoning(rs::ReasoningItem {
            id: "r".into(),
            summary: vec![],
            content: None,
            encrypted_content: None,
            status: None,
        });
        let empty = ConversationItem::Reasoning(rs::ReasoningItem {
            id: "r".into(),
            summary: vec![],
            content: Some(vec![]),
            encrypted_content: None,
            status: None,
        });
        assert_ne!(
            raw_fingerprint(&absent),
            raw_fingerprint(&empty),
            "joint PIN Reasoning.content presence pair (n+s): None ≠ Some([])"
        );
    }

    /// Cross-variant discriminant (both action tags). Kept alongside per-tag
    /// presence pins; not a substitute for Find.url / Find.pattern only-diffs.
    #[test]
    fn pin_websearch_open_page_vs_search() {
        let search = web_search("ws_1", "q", rs::WebSearchToolCallStatus::Completed);
        let open = ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: "ws_1".into(),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::OpenPage(rs::WebSearchActionOpenPage {
                    url: Some("https://example.com".into()),
                }),
            }),
        });
        assert_fp_diff(search, open, "WebSearch.action OpenPage vs Search");
    }

    #[test]
    fn pin_websearch_find_vs_find_in_page() {
        assert_fp_diff(
            find_action("https://example.com", "p"),
            find_in_page_action("https://example.com", "p"),
            "WebSearch.action Find vs FindInPage",
        );
    }

    // ── U2: Option absent vs present-empty ───────────────────────────────

    #[test]
    fn pin_option_assistant_model_id_absent_vs_empty() {
        let mut a = base_assistant();
        let mut b = base_assistant();
        a.model_id = None;
        b.model_id = Some(String::new());
        assert_fp_diff(
            ConversationItem::Assistant(a),
            ConversationItem::Assistant(b),
            "Assistant.model_id None vs Some(\"\")",
        );
    }

    #[test]
    fn pin_option_assistant_model_fingerprint_absent_vs_empty() {
        let mut a = base_assistant();
        let mut b = base_assistant();
        a.model_fingerprint = None;
        b.model_fingerprint = Some(String::new());
        assert_fp_diff(
            ConversationItem::Assistant(a),
            ConversationItem::Assistant(b),
            "Assistant.model_fingerprint None vs Some(\"\")",
        );
    }

    #[test]
    fn pin_option_reasoning_encrypted_absent_vs_empty() {
        let mk = |enc: Option<String>| {
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r".into(),
                summary: vec![],
                content: None,
                encrypted_content: enc,
                status: None,
            })
        };
        assert_fp_diff(
            mk(None),
            mk(Some(String::new())),
            "Reasoning.encrypted None vs Some(\"\")",
        );
    }

    #[test]
    fn pin_option_reasoning_content_absent_vs_empty_vec() {
        let mk = |content: Option<Vec<rs::ReasoningTextContent>>| {
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r".into(),
                summary: vec![],
                content,
                encrypted_content: None,
                status: None,
            })
        };
        assert_fp_diff(
            mk(None),
            mk(Some(vec![])),
            "Reasoning.content None vs Some([])",
        );
    }

    #[test]
    fn pin_option_websearch_sources_absent_vs_empty_vec() {
        let absent = ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: "ws_1".into(),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                    query: "q".into(),
                    sources: None,
                }),
            }),
        });
        let empty = web_search_with_sources("ws_1", "q", vec![]);
        assert_fp_diff(absent, empty, "WebSearch.sources None vs Some([])");
    }

    #[test]
    fn pin_option_openpage_url_absent_vs_empty() {
        let mk = |url: Option<String>| {
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                    id: "ws_1".into(),
                    status: rs::WebSearchToolCallStatus::Completed,
                    action: rs::WebSearchToolCallAction::OpenPage(rs::WebSearchActionOpenPage {
                        url,
                    }),
                }),
            })
        };
        assert_fp_diff(
            mk(None),
            mk(Some(String::new())),
            "OpenPage.url None vs Some(\"\")",
        );
    }

    #[test]
    fn pin_option_codeinterp_code_absent_vs_empty() {
        let mk = |code: Option<String>| {
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::CodeInterpreter(rs::CodeInterpreterToolCall {
                    id: "ci_1".into(),
                    container_id: "ctr".into(),
                    code,
                    outputs: None,
                    status: rs::CodeInterpreterToolCallStatus::Completed,
                }),
            })
        };
        assert_fp_diff(
            mk(None),
            mk(Some(String::new())),
            "CodeInterp.code None vs Some(\"\")",
        );
    }

    #[test]
    fn pin_option_codeinterp_outputs_null_vs_empty_vec() {
        let mk = |outputs: Option<Vec<rs::CodeInterpreterToolCallOutput>>| {
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::CodeInterpreter(rs::CodeInterpreterToolCall {
                    id: "ci_1".into(),
                    container_id: "ctr".into(),
                    code: Some("x".into()),
                    outputs,
                    status: rs::CodeInterpreterToolCallStatus::Completed,
                }),
            })
        };
        assert_fp_diff(mk(None), mk(Some(vec![])), "CodeInterp.outputs null vs []");
    }

    #[test]
    fn pin_option_user_synthetic_reason_absent_vs_present() {
        use xai_grok_sampling_types::SyntheticReason;
        let mut a = base_user();
        let mut b = base_user();
        // user vs user_meta changes kind tag too; keep both as user_meta-capable
        // by using CompactionMeta vs None on items that stay "user" when None —
        // kind prefix differs (user vs user_meta). Pin presence on meta arm:
        a.synthetic_reason = None;
        b.synthetic_reason = Some(SyntheticReason::CompactionMeta);
        // Kind tag differs by design (user vs user_meta) — still a real wire
        // difference the fingerprint must see.
        assert_fp_diff(
            ConversationItem::User(a),
            ConversationItem::User(b),
            "User.synthetic_reason None vs Some",
        );
    }

    #[test]
    fn pin_option_reasoning_status_absent_vs_present() {
        let mk = |status: Option<rs::OutputStatus>| {
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r".into(),
                summary: vec![],
                content: None,
                encrypted_content: None,
                status,
            })
        };
        assert_fp_diff(
            mk(None),
            mk(Some(rs::OutputStatus::Completed)),
            "Reasoning.status None vs Some",
        );
    }

    #[test]
    fn pin_option_assistant_reasoning_effort_absent_vs_present() {
        use xai_grok_sampling_types::ReasoningEffort;
        let mut a = base_assistant();
        let mut b = base_assistant();
        a.reasoning_effort = None;
        b.reasoning_effort = Some(ReasoningEffort::Low);
        assert_fp_diff(
            ConversationItem::Assistant(a),
            ConversationItem::Assistant(b),
            "Assistant.reasoning_effort None vs Some",
        );
    }
}
