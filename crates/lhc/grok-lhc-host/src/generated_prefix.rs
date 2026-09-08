//! LHC-generated write-back prefix (slice 1A).
//!
//! After an LHC compact the host installs LHC's generated body (band + served
//! tail) as the native conversation. That body is **not** canonical source:
//! the capture worker keeps its item digests and the canonical event order it
//! was generated from (`source_tip`), skips it on every whole-history re-map
//! (bootstrap / `ReplaceHistory`), and keys the genuine remainder from the
//! frozen pre-write-back occurrence baseline — the stored keys at or before
//! `source_tip`. From that seed the remainder reproduces the keys live capture
//! minted, so captured suffix dedups and uncaptured suffix records, on every
//! re-map and every restart alike.
//!
//! The walk is ordered and exact. Two in-place native rewrites are allowed
//! inside the prefix: the memory-reminder upsert of the System head (mapped
//! as a changed head) and a tool-result prune with the same call id (skipped;
//! the original result is already archived). Anything else stops the walk and
//! the caller falls back to the full re-map (today's behaviour) with a
//! greppable warning — see [`WALK_STOPPED_EARLY`].

use xai_grok_sampling_types::ConversationItem;

use crate::idempotency::{OccurrenceTracker, item_digest, seed_occurrence_from_keys};
use crate::session::LhcSession;

/// Fixed prefix of the early-stop warning (C1): grep for it in live logs.
pub const WALK_STOPPED_EARLY: &str = "LHC writeback prefix walk stopped early";

/// An LHC-generated body installed as native history, with the canonical tip
/// it was generated from. Produced by the write-back (in process) or read back
/// from the LHC-marked compaction checkpoint (resume).
#[derive(Debug, Clone)]
pub struct GeneratedPrefix {
    pub items: Vec<ConversationItem>,
    pub source_tip: u64,
}

/// Outcome of walking a native slice against the installed prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrefixWalk {
    /// The generated prefix is fully represented. `remainder` is the first
    /// genuine index; `remap_head` says `native[0]` is a rewritten System head
    /// that must be mapped as well.
    Covered { remap_head: bool, remainder: usize },
    /// The walk stopped before the end of the generated prefix.
    Stopped { index: usize, cause: &'static str },
}

/// Worker-side state for an installed generated prefix.
#[derive(Debug)]
pub(crate) struct InstalledPrefix {
    body: Vec<ConversationItem>,
    digests: Vec<String>,
    source_tip: u64,
    /// Frozen baseline (keys at or before `source_tip`), read once on demand.
    seed: Option<OccurrenceTracker>,
}

impl InstalledPrefix {
    pub(crate) fn new(generated: GeneratedPrefix) -> Self {
        let digests = generated.items.iter().map(item_digest).collect();
        Self {
            body: generated.items,
            digests,
            source_tip: generated.source_tip,
            seed: None,
        }
    }

    pub(crate) fn source_tip(&self) -> u64 {
        self.source_tip
    }

    pub(crate) fn len(&self) -> usize {
        self.digests.len()
    }

    /// Ordered exact walk of `native` against the generated prefix.
    pub(crate) fn walk(&self, native: &[ConversationItem]) -> PrefixWalk {
        let mut remap_head = false;
        for (i, expected) in self.digests.iter().enumerate() {
            let Some(item) = native.get(i) else {
                // Native shorter than the prefix and equal so far: a rewind
                // into the body. Nothing beyond it to map.
                return PrefixWalk::Covered {
                    remap_head,
                    remainder: native.len(),
                };
            };
            if item_digest(item) == *expected {
                continue;
            }
            match (item, &self.body[i]) {
                // Memory-reminder upsert rewrites the existing System head in
                // place: capture the changed head, keep walking.
                (ConversationItem::System(_), ConversationItem::System(_)) if i == 0 => {
                    remap_head = true;
                }
                // Tool-result prune (soft trim / hard clear) rewrites content
                // in place under the same call id; the original is archived.
                (ConversationItem::ToolResult(a), ConversationItem::ToolResult(b))
                    if a.tool_call_id == b.tool_call_id => {}
                _ => {
                    return PrefixWalk::Stopped {
                        index: i,
                        cause: "digest_mismatch",
                    };
                }
            }
        }
        PrefixWalk::Covered {
            remap_head,
            remainder: self.digests.len(),
        }
    }

    /// The frozen occurrence baseline: stored keys with event order at or
    /// before `source_tip`. Immutable once written, so read once per prefix.
    pub(crate) async fn frozen_seed(
        &mut self,
        sess: &LhcSession,
    ) -> Result<OccurrenceTracker, String> {
        if let Some(seed) = &self.seed {
            return Ok(seed.clone());
        }
        let events = sess.list_events().await?;
        let tip = i64::try_from(self.source_tip).unwrap_or(i64::MAX);
        let seed = seed_occurrence_from_keys(
            events
                .iter()
                .filter(|e| e.event_order() <= tip)
                .map(|e| e.idempotency_key()),
        );
        self.seed = Some(seed.clone());
        Ok(seed)
    }
}

#[cfg(test)]
mod tests {
    use xai_grok_sampling_types::{ConversationItem, ToolCall};

    use super::*;

    fn body() -> Vec<ConversationItem> {
        vec![
            ConversationItem::system("sys"),
            ConversationItem::user_meta("[context · brief] band"),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }]),
            ConversationItem::tool_result("c1", "long result"),
            ConversationItem::assistant("done"),
        ]
    }

    fn installed() -> InstalledPrefix {
        InstalledPrefix::new(GeneratedPrefix {
            items: body(),
            source_tip: 7,
        })
    }

    #[test]
    fn exact_body_plus_suffix_is_covered() {
        let mut native = body();
        native.push(ConversationItem::user("next"));
        assert_eq!(
            installed().walk(&native),
            PrefixWalk::Covered {
                remap_head: false,
                remainder: 5
            }
        );
    }

    #[test]
    fn rewritten_system_head_is_remapped_and_walk_continues() {
        let mut native = body();
        native[0] = ConversationItem::system("sys + memory reminder");
        assert_eq!(
            installed().walk(&native),
            PrefixWalk::Covered {
                remap_head: true,
                remainder: 5
            }
        );
    }

    #[test]
    fn pruned_tool_result_same_call_id_is_skipped() {
        let mut native = body();
        native[3] = ConversationItem::tool_result("c1", "[cleared]");
        assert_eq!(
            installed().walk(&native),
            PrefixWalk::Covered {
                remap_head: false,
                remainder: 5
            }
        );
    }

    #[test]
    fn rewind_into_body_is_covered_with_nothing_beyond() {
        let native = body()[..3].to_vec();
        assert_eq!(
            installed().walk(&native),
            PrefixWalk::Covered {
                remap_head: false,
                remainder: 3
            }
        );
    }

    /// Named limitation: subagent with an unreleased inherited prefix restarts
    /// with a body that differs from the checkpoint at index 0.
    #[test]
    fn inherited_prefix_shape_stops_at_index_zero() {
        let mut native = body();
        native[0] = ConversationItem::user("inherited head, not a System item");
        assert_eq!(
            installed().walk(&native),
            PrefixWalk::Stopped {
                index: 0,
                cause: "digest_mismatch"
            }
        );
    }

    /// Named limitation: an image strip / removal inside the generated body.
    #[test]
    fn image_strip_shape_inside_body_stops_mid_walk() {
        let mut native = body();
        native.remove(2);
        assert_eq!(
            installed().walk(&native),
            PrefixWalk::Stopped {
                index: 2,
                cause: "digest_mismatch"
            }
        );
    }

    /// A different tool result under a different call id is not a prune.
    #[test]
    fn tool_result_with_other_call_id_stops() {
        let mut native = body();
        native[3] = ConversationItem::tool_result("c2", "long result");
        assert!(matches!(
            installed().walk(&native),
            PrefixWalk::Stopped { index: 3, .. }
        ));
    }
}
