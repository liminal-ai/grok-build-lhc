//! Grok Build host adapter for LHC (fork-only crate; see /FORK.md).
//!
//! Chunk 0 state: packaging skeleton only. Chunk 1 adds the event-capture
//! mapping (ConversationItem -> MessageEventInput), session/thread lifecycle,
//! and idempotency keys; Chunk 2 adds the ModelCall adapter and the
//! compact/request-context bridge.
//!
//! This crate is the compile-layer tripwire: it consumes the vendored `lhc`
//! crate (and, from Chunk 1 on, the host crates it seams into), so upstream
//! or vendor drift at a seam surfaces as a build failure in
//! `scripts/check-lhc-hooks.sh`, not as silent capture loss.

/// Re-exported so the tripwire compile genuinely links the vendored port.
pub use lhc::sdk::init_lhc;

#[cfg(test)]
mod tests {
    /// Vendored-port linkage smoke: the adapter can see the SDK surface.
    #[test]
    fn vendored_lhc_links() {
        // A type-level touch is enough — behavior is certified in the
        // port's own repo (481-test gate), not re-tested here.
        let _ = super::init_lhc;
    }
}
