# Golden event transcripts

JSON files in this directory are canonical LHC intake shapes produced by the
Chunk 1 mapper. Loaded by `tests/golden_smoke.rs`.

Each file is an array of objects with:
- `eventKind`
- `idempotencyKey`
- `actor` / `harness`
- `payload`

Fixtures are hand-authored (or refreshed only when a developer explicitly sets
`UPDATE_GOLDENS=1`). CI never sets that variable. Payload shapes are also
anchored independently by deserializing into LHC's `deny_unknown_fields` types.

Caveats (do not over-read): fixtures originate from the mapper under test when
regenerated, and the projection omits `MessageEventInput.extra` (always empty
in Chunk 1). Treat them as shape + key + payload anchors, not as an independent
oracle of mapper correctness.
