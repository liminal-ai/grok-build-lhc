//! Chunk 2 seam: inert / erroring inference callbacks for Chunk 1 capture.
//!
//! `init_lhc` requires exactly one of `inference` / `inference_callbacks`.
//! Capture does not depend on inference; these callbacks refuse classified
//! so accidental drain cannot invent content.

use std::sync::Arc;

use lhc::sdk::{InferenceCallbacks, InferenceResult};

const CHUNK2_REASON: &str = "Chunk 2 seam: inference adapter not yet wired";

fn refuse() -> InferenceResult {
    InferenceResult::Err {
        reason: CHUNK2_REASON.to_string(),
        request_messages: None,
    }
}

/// Build inference callbacks that always error with a Chunk-2-classified reason.
pub fn chunk2_stub_inference_callbacks() -> InferenceCallbacks {
    InferenceCallbacks {
        smooth_prompt: Arc::new(|_i| Box::pin(async { refuse() })),
        summarize_tool_result: Arc::new(|_i| Box::pin(async { refuse() })),
        compress_detailed_turn: Arc::new(|_i| Box::pin(async { refuse() })),
        summarize_chunk_brief: Arc::new(|_i| Box::pin(async { refuse() })),
    }
}
