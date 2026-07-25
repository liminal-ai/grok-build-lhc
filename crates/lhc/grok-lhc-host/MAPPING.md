# ConversationItem → LHC MessageEventInput mapping

Chunk 1 capture vocabulary. Every host enum variant is matched exhaustively in
`src/mapping.rs` (no `_ =>` wildcards on host enums). Target kinds are the
closed LHC `EVENT_KINDS` (9).

Actor / harness stamped on every event: `actor = "grok"`, `harness = "grok-build"`.

## `ConversationItem`

| Host variant | LHC event kind(s) | Payload notes |
|---|---|---|
| `System` | `runtime_note` | `text` = system content. Not a user prompt. |
| `User` with `synthetic_reason: None` | `user_prompt` | Real user input. `text` from typed `ContentPart`s (see below). |
| `User` with `SyntheticReason` | see table below | Driven by `SyntheticReason::starts_prompt_turn()`. |
| `Assistant` | `assistant_text` (if content non-empty, or no tool calls); one `tool_call` per `ToolCall`; `turn_end` when `tool_calls` is empty | Client-executed tool calls only on this item. |
| `ToolResult` | `tool_result` | `tool_call_id` + `content` (+ image parts folded into text). `is_error` omitted (`None`) — host `ToolResultItem` does not persist an error flag. |
| `BackendToolCall` | `tool_call` **and** paired `tool_result` | Server-side tools are recorded as a call plus an immediate result carrying status/outputs (see below). |
| `Reasoning` | `assistant_thinking` | `text` from `reasoning_item_text` (summary + content parts). |

## `SyntheticReason`

Mapping is exhaustive and driven by the host classifier
`SyntheticReason::starts_prompt_turn()` (`conversation.rs`).

| Variant | LHC event(s) | Rationale |
|---|---|---|
| `TaskCompleted` | `turn_end` then `runtime_note` | `starts_prompt_turn() == true`. Closes a prior (possibly aborted) turn; note is not real user input. |
| `SubagentCompleted` | `turn_end` then `runtime_note` | same |
| `NotificationDrain` | `turn_end` then `runtime_note` | same |
| `GoalClassifierNudge` | `turn_end` then `runtime_note` | same |
| `SchedulerFired` | `turn_end` then `runtime_note` | same |
| `CompactionMeta` | `runtime_note` | Compaction pipeline metadata, not user input. |
| `SystemReminder` | `runtime_note` | Runtime `<system-reminder>`, not user input. |
| `ProjectInstructions` | `runtime_note` | Injected project instructions (AGENTS.md / CLAUDE.md). |
| `AutoContinue` | `runtime_note` | Post-compaction continue injection. |
| `AutoRecovery` | `runtime_note` | Transient-failure retry injection. |
| `Interjection` | `runtime_note` | Mid-turn Ctrl+Enter steering; host never consumed a `prompt_index` — **no turn boundary** (Ruling R1). |
| `GoalSummary` | `runtime_note` | Tags both a legacy turn and an in-turn directive — **no turn boundary** (Ruling R1). |
| `StopHookFeedback` | `runtime_note` | Stop-hook feedback — still synthetic. |
| `WorkingDirectorySwitch` | `runtime_note` | CWD relocation reminder (also via ack path). |
| `Unknown` | `runtime_note` | Explicit forward-compat arm; treat as runtime note, never silent drop. |

`turn_end` (not `user_prompt`) is used for turn-starting synthetics because LHC
`turns::create` opens a new turn on either `turn_end` or `user_prompt`, while
these items are explicitly *not* real user input.

## `ContentPart` (user / tool-result images)

| Variant | Encoding into `text` |
|---|---|
| `Text { text }` | Appended as-is (joined with `\n` when multiple parts). |
| `Image { url }` | Appended as `[image:{preview}]` where `preview` is the URL truncated to 128 chars, with a `…({nbytes}B)` suffix when truncated. Full base64 data URLs must not inflate LHC text payloads. |

## `BackendToolKind` → `tool_call` + `tool_result`

Every backend call emits a paired `tool_result` so LHC does not record a
server-side tool with no result. Result `content` is a JSON string of status
(and outputs when present). For `CodeInterpreter` only, `isError` is set from
`status == Failed` (the host exposes status on that arm). Client `ToolResult`
items still omit `isError`.

| Variant | `tool_name` | `arguments` | result content |
|---|---|---|---|
| `WebSearch` | `web_search` | JSON object of the typed action | `{ status, action }` |
| `XSearch` | `x_search` (or `CustomToolCall.name`) | `{ "input": ... }` | `{ status: "server_executed", callId, input }` — CustomToolCall has no embedded outputs |
| `CodeInterpreter` | `code_interpreter` | `{ code, containerId }` | `{ status, outputs }` |

## Model / thinking changes (not via persistence)

Emitted from the model-switch hook (`capture_model_or_thinking_change`). The
host passes the authoritative previous model id and previous reasoning effort.
No-op transitions (same model and same level) are suppressed — including the
session-start re-selection path where `apply` runs with an unchanged model.

| Condition | LHC kind | Payload |
|---|---|---|
| Model id changed | `model_change` | `previousModel` / `newModel` |
| Reasoning effort changed | `thinking_level_change` | `previousLevel` / `newLevel` (`"none"` when unset) |

`last_model` / in-memory previous-state tracking is not used; the host values
are authoritative on every call. Ordinals advance only after
`submit_events` reports `Recorded`.

## `turn_end`

Emitted after:

1. An `Assistant` item whose `tool_calls` is empty (terminal assistant), or
2. A turn-starting synthetic (see table) — closes a prior turn that may have
   been aborted with tool calls outstanding.

**Open case (documented):** a turn aborted with tool calls outstanding and
*without* a subsequent turn-starting synthetic leaves the LHC turn open until
the next real `user_prompt` or terminal toolless `Assistant`. Banded compaction
sees a long turn in that window. Certification covers the synthetic-wake close
path; the pure-abort-without-wake path is intentional host silence.

## `replace_history` (Chunk 1 decision — B1)

Host `replace_history` is **not** compaction/rewind-only. It is also the sole
persistence path for:

- dangling-tool / duplicate-tool-result **repairs** (`actor/mutations.rs`) —
  synthetic `ToolResult` items
- **memory-reminder** injection (`actor/request_builder.rs`) — prepended `System`
- **compaction** (`replace_conversation`) — including the `CompactionMeta` summary

So the adapter **submits the mapped replacement slice**. LHC's own
`DuplicateIdempotencyKey` dedup is the diff engine: survivors mint identical
keys and are skipped; genuinely new items (repairs, reminders, summaries) are
recorded. There is no generation bump on replace (that was the round-1
amplification).

Occurrence tracking is **monotonic**: a digest's high-water mark never moves
downward on realignment. After `[A,B]` → rewind to `[A]`, `B`'s counter stays
raised, so a re-sent identical `B` takes the next occurrence and is recorded.
The tracker is **seeded from LHC's stored event keys at open** (not from the
host bootstrap slice), so this survives process restart even when the host
conversation no longer contains `B`.

A dropped `ReplaceHistory` (full queue) poisons the baseline and refuses further
persists until a successful replace.

**Bootstrap:** always submits the mapped bootstrap slice under stable item
generation `0`; LHC dedup skips already-captured keys. The occurrence tracker
is seeded from stored events first, then `merge_monotonic` with the bootstrap
local map.

## Idempotency keys

```
grok:{session}:g{generation}:{item_blake3}:{occurrence}:{event_kind}[:{part}]
grok:{session}:model_change:{ordinal}:{prev}:{new}
grok:{session}:thinking_level_change:{ordinal}:{prev}:{new}
```

- `session` — ACP session id with `:` / `%` escaped (`%3A` / `%25`) so colons
  cannot shift key fields
- `generation` — **stable `0` for conversation-item keys** (B1). Tip
  (`last_event_order`) is still tracked on the session for diagnostics and is
  latched with `max(old, batch.tip)`, but it is not mixed into item keys (doing
  so would re-key survivors on every replace).
- `item_blake3` — blake3 of canonical `serde_json` bytes for the
  `ConversationItem` (serialize failure → warn + Debug-based fallback digest;
  no ASLR pointers)
- `occurrence` — 0-based emit count per digest. **Monotonic** high-water marks
  seeded from LHC stored keys at open; `replace_history` uses a local map for
  submit keys then `merge_monotonic`. The counter advances before submit — a
  failed submit leaves a gap (retry cannot collide with a partial apply). Keys
  never use wall clock or RNG.
- `part` — e.g. tool call id, `turn_end`, or `pre_synthetic`
- `ordinal` — monotonic per-session model/thinking change counter, seeded on
  open from `list_events` so toggle cycles (`none→high→none→high`) never collide
  across restart

## Thread open / resolve

Existing sessions are opened via `threads.info(filePath)` + registry
`resolve` / `list_threads` (F8). Identity rules:

- **Orphan** (file exists, no registry binding via resolve or `list_threads`
  file-path match): **refuse**.
- **Registry/file disagreement** (resolve succeeds but registry `file_path`
  is not this session's layout path): **refuse**. Adopting the registry path
  would silently attach this ACP session to another transcript — unsafe.
- **`list_threads` fallback**: when `resolve(thread_id)` fails but
  `list_threads` finds a row whose `file_path` matches this session's file,
  reopen that path (prefix-id / resolve ambiguity recovery only — still the
  same file, not a different transcript).

Generation / occurrence / change-ordinal tip are read from `list_events` at
open; failure is **fatal** (refuse open) — never assume zeros (B2). Legacy
`{thread}.meta.json` files from round 1 are deleted on open.

Thread filenames use injective percent-encoding of the ACP session id (B4)
so `a:b` and `a_b` cannot share `grok-….sqlite`.

## Capture queue

Bounded (1024), non-blocking `try_send`. On full queue the event is dropped, a
process-local counter increments, and a depth gauge is logged. `model_change`
drops are counted the same way. A dropped `replace_history` poisons the
baseline (see above). Must not block the chat-state actor. `dropped_count()` /
`queue_depth()` are available on the handle.

## Gate

`GROK_LHC` is consulted once at tee install (`tee_chat_persistence`).
Post-spawn callbacks use registry presence only — they do not re-read the
environment.
