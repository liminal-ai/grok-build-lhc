//! Stable idempotency keys for LHC capture.
//!
//! Key shape:
//! ```text
//! grok:{session}:g{generation}:{digest}:{occurrence}:{event_kind}[:{part}]
//! grok:{session}:model_change:{ordinal}:{prev}:{new}
//! grok:{session}:thinking_level_change:{ordinal}:{prev}:{new}
//! ```
//!
//! `session` is colon-escaped (`:` → `%3A`) so ACP session ids containing `:`
//! cannot break ordinal seeding or key parsing.
//!
//! Conversation-item keys use a **stable** `generation` (always `0` under B1) so
//! `replace_history` re-submits mint identical keys for survivors and LHC
//! dedup is the diff engine. `occurrence` is monotonic and seeded from LHC's
//! stored events at open — never from the host bootstrap slice alone.

use std::collections::HashMap;

use blake3::Hasher;
use tracing::warn;
use xai_grok_sampling_types::ConversationItem;

/// Stable generation embedded in conversation-item keys.
///
/// Must not change across `replace_history` or tip advances — otherwise
/// survivors would re-key and amplify the transcript.
pub const ITEM_KEY_GENERATION: u64 = 0;

/// Running occurrence counter keyed by item digest.
///
/// High-water marks only move upward (`merge_monotonic` / `next`). Seeded from
/// LHC stored event keys at open so rewind/prune cannot forget prior emits.
#[derive(Debug, Default, Clone)]
pub struct OccurrenceTracker {
    /// Next occurrence to allocate for each digest (i.e. count of prior emits).
    counts: HashMap<String, u64>,
}

impl OccurrenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next occurrence index for this item digest.
    pub fn next(&mut self, digest: &str) -> u64 {
        let entry = self.counts.entry(digest.to_string()).or_insert(0);
        let occ = *entry;
        *entry += 1;
        occ
    }

    /// Raise each digest's high-water mark to at least `other`'s.
    ///
    /// Never moves a counter downward — required so rewind then re-send of an
    /// identical item allocates a fresh occurrence.
    pub fn merge_monotonic(&mut self, other: &OccurrenceTracker) {
        for (digest, &other_next) in &other.counts {
            let entry = self.counts.entry(digest.clone()).or_insert(0);
            *entry = (*entry).max(other_next);
        }
    }

    /// Observe that occurrence `occ` was already used for `digest`.
    pub fn observe(&mut self, digest: &str, occ: u64) {
        let entry = self.counts.entry(digest.to_string()).or_insert(0);
        *entry = (*entry).max(occ.saturating_add(1));
    }
}

/// Escape `:` in session ids so key structure stays parseable.
pub fn encode_session_id(session_id: &str) -> String {
    session_id.replace('%', "%25").replace(':', "%3A")
}

/// Blake3 hex digest of the canonical JSON encoding of `item`.
///
/// On serialization failure, warns and falls back to a Debug-based
/// discriminator (restart-stable within a process family; no ASLR pointers).
pub fn item_digest(item: &ConversationItem) -> String {
    match serde_json::to_vec(item) {
        Ok(bytes) => {
            let mut hasher = Hasher::new();
            hasher.update(&bytes);
            hasher.finalize().to_hex().to_string()
        }
        Err(err) => {
            warn!(
                ?err,
                "LHC: ConversationItem serialize failed; using fallback digest"
            );
            let mut hasher = Hasher::new();
            hasher.update(b"serialize-failed:");
            hasher.update(format!("{item:?}").as_bytes());
            hasher.finalize().to_hex().to_string()
        }
    }
}

/// Build a capture idempotency key for a mapped sub-event of a conversation item.
pub fn item_event_key(
    session_id: &str,
    generation: u64,
    digest: &str,
    occurrence: u64,
    event_kind: &str,
    part: Option<&str>,
) -> String {
    let sid = encode_session_id(session_id);
    match part {
        Some(part) => {
            format!("grok:{sid}:g{generation}:{digest}:{occurrence}:{event_kind}:{part}")
        }
        None => {
            format!("grok:{sid}:g{generation}:{digest}:{occurrence}:{event_kind}")
        }
    }
}

/// Model-change key with monotonic per-session ordinal (survives restart via seeding).
pub fn model_change_key(session_id: &str, ordinal: u64, previous: &str, new: &str) -> String {
    let sid = encode_session_id(session_id);
    format!("grok:{sid}:model_change:{ordinal}:{previous}:{new}")
}

/// Thinking-level-change key with monotonic per-session ordinal.
pub fn thinking_level_change_key(
    session_id: &str,
    ordinal: u64,
    previous: &str,
    new: &str,
) -> String {
    let sid = encode_session_id(session_id);
    format!("grok:{sid}:thinking_level_change:{ordinal}:{previous}:{new}")
}

/// Parse the highest model/thinking change ordinal already present in stored keys.
pub fn max_change_ordinal_from_keys<'a>(keys: impl IntoIterator<Item = &'a str>) -> u64 {
    let mut max = 0u64;
    for key in keys {
        for marker in [":model_change:", ":thinking_level_change:"] {
            if let Some(idx) = key.find(marker) {
                let rest = &key[idx + marker.len()..];
                if let Some(ord_str) = rest.split(':').next()
                    && let Ok(ord) = ord_str.parse::<u64>()
                {
                    max = max.max(ord.saturating_add(1));
                }
            }
        }
    }
    max
}

/// Seed an occurrence tracker from stored LHC idempotency keys.
///
/// Parses `grok:{sid}:g{gen}:{digest}:{occ}:{kind}…` and raises high-water
/// marks. Model/thinking keys are ignored.
pub fn seed_occurrence_from_keys<'a>(keys: impl IntoIterator<Item = &'a str>) -> OccurrenceTracker {
    let mut tracker = OccurrenceTracker::new();
    for key in keys {
        if let Some((digest, occ)) = parse_item_key_digest_occ(key) {
            tracker.observe(&digest, occ);
        }
    }
    tracker
}

fn parse_item_key_digest_occ(key: &str) -> Option<(String, u64)> {
    // grok:{sid}:g{gen}:{digest}:{occ}:{event_kind}[:part]
    let rest = key.strip_prefix("grok:")?;
    // sid may contain %3A but not unencoded ':'
    let after_sid = rest.split_once(':')?.1;
    let after_g = after_sid.strip_prefix('g')?;
    // gen:digest:occ:kind...
    let mut parts = after_g.splitn(4, ':');
    let _gen = parts.next()?;
    let digest = parts.next()?;
    let occ_str = parts.next()?;
    let _kind = parts.next()?;
    let occ: u64 = occ_str.parse().ok()?;
    if digest.is_empty() {
        return None;
    }
    Some((digest.to_string(), occ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::ConversationItem;

    #[test]
    fn identical_items_get_distinct_occurrence_keys() {
        let mut tracker = OccurrenceTracker::new();
        let item = ConversationItem::user("hello");
        let d = item_digest(&item);
        let o0 = tracker.next(&d);
        let o1 = tracker.next(&d);
        assert_eq!(o0, 0);
        assert_eq!(o1, 1);
        let k0 = item_event_key("s1", 0, &d, o0, "user_prompt", None);
        let k1 = item_event_key("s1", 0, &d, o1, "user_prompt", None);
        assert_ne!(k0, k1);
    }

    #[test]
    fn merge_monotonic_never_decreases() {
        let mut t = OccurrenceTracker::new();
        let d = "abcd";
        assert_eq!(t.next(d), 0);
        assert_eq!(t.next(d), 1);
        let mut local = OccurrenceTracker::new();
        assert_eq!(local.next(d), 0); // slice remap
        t.merge_monotonic(&local);
        assert_eq!(t.next(d), 2, "high-water must survive slice realignment");
    }

    #[test]
    fn seed_from_keys_raises_high_water() {
        let d = "deadbeef";
        let k0 = item_event_key("s", 0, d, 0, "user_prompt", None);
        let k1 = item_event_key("s", 0, d, 1, "user_prompt", None);
        let mut t = seed_occurrence_from_keys([k0.as_str(), k1.as_str()]);
        assert_eq!(t.next(d), 2);
    }

    #[test]
    fn model_ordinal_avoids_toggle_collision() {
        let a = model_change_key("s", 0, "none", "high");
        let b = model_change_key("s", 1, "high", "none");
        let c = model_change_key("s", 2, "none", "high");
        assert_ne!(a, c);
        assert_ne!(a, b);
    }

    #[test]
    fn max_change_ordinal_seeds_from_keys() {
        let keys = [
            "grok:s:model_change:0:a:b",
            "grok:s:thinking_level_change:1:none:high",
            "grok:s:g0:deadbeef:0:user_prompt",
        ];
        assert_eq!(max_change_ordinal_from_keys(keys), 2);
    }

    #[test]
    fn colon_session_id_does_not_break_ordinal_seeding() {
        let sid = "acp:session:with:colons";
        let k0 = model_change_key(sid, 0, "none", "high");
        let k1 = thinking_level_change_key(sid, 1, "high", "none");
        assert!(k0.contains("%3A"), "session id must be escaped: {k0}");
        assert_eq!(max_change_ordinal_from_keys([k0.as_str(), k1.as_str()]), 2);
    }

    #[test]
    fn item_digest_is_content_addressed_not_identity() {
        // The key design requires the digest to be a pure function of content:
        // two independently allocated items with equal content must agree, or a
        // replayed slice mints fresh keys and dedup stops working.
        let a = ConversationItem::user("same text");
        let b = ConversationItem::user("same text");
        let c = ConversationItem::user("other text");
        assert_eq!(item_digest(&a), item_digest(&b));
        assert_ne!(item_digest(&a), item_digest(&c));
    }
}
