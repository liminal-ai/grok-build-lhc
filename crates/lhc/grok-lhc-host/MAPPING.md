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

### Identical-content dedup — correct by design (ruling)

A summary (or any item) that **serializes byte-identically** to an
already-recorded item is skipped by LHC `DuplicateIdempotencyKey` dedup.
**This is the content-addressed B1 contract working, not an accident.** The
record is **content-complete**: a byte-identical summary adds no information
to the event log. Compaction *provenance* does **not** live in the event log.

| Provenance question | Where it lives |
|---|---|
| Compact receipts (`view_id`, `profile`, `config`, bands report, `tail_tokens`, `total_tokens`, `covered_from`, `compact_point`, `degraded`, …) | `CompactReceipt` — `vendor/.../thread_view/mod.rs` (~1235–1250) / `shared_tech/view.rs` |
| Derivation family | `query_derivation_log` → `Vec<StoredDerivationLogEntry>` — `vendor/.../sdk.rs` (~230) |

"Was this text produced by a compaction, and under what profile/config?" is
answered from the receipt and derivation log, not from a duplicate event.
Pinned by `identical_content_summary_dedup_is_inert` (no new event, no
key-stream disturbance for subsequent items, occurrence walk aligned).
**Do not change this dedup behavior without a new ruling.**

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

## Chunk 2 — request-context serving (hook 4 in shell `turn.rs`)

After host `ChatStateHandle::build_request` returns, the shell may substitute
`request.items` from LHC `get_llm_request_context`. Tools, model, sampling
params, and `prompt_index` / `x_grok_turn_idx` stay host-owned. Fail-open to
the native body; never hybrid (all-LHC body or all-native body).

### Post-substitution accounting (ruling)

| Item | Decision | Test |
|---|---|---|
| **Token accounting** | `ConversationRequest` has no body-token field; the provider counts from the substituted wire body. Host `total_tokens` / auto-compact triggers remain on the **native** actor conversation and are **not** rewritten to match the LHC body (rewriting them would desync compaction from persistence). | (ruling; covered by write-back token decrease + accounting_* suite — no separate substitute-totals test) |
| **Image byte-budget eviction** | Ran on the native clone inside `build_request`. On Substitute that clone is discarded; the wire body is the LHC text view. Native eviction is **moot for the wire**. | `accounting_image_eviction_moot_on_substitute_wire` |
| **Memory-reminder injection** | Lives in the leading native `System` item(s). Serving **preserves** that system prefix and replaces only the non-system body. | `accounting_memory_reminder_preserved_in_system_prefix` |
| **Integrity repair** | Ran on the actor before `build_request`. Live-tail tool calls/results are **conserved** on substitute/write-back (structured `tool_calls` + `ToolResult`); bands stay prose. Mid-cycle native bodies are still replaced wholesale (all-LHC, never hybrid). | `tool_result_is_conserved_as_tool_result_item`, `assistant_tool_call_is_conserved_on_assistant_item`, `writeback_live_tail_kinds_round_trip` |
| **`prompt_index` / `UserItem::prompt_index`** | Serving **and** write-back stamp from the **tail** of native markers (`assign_prompt_indices_from_tail`): LHC real users are the live suffix after compact, so head assignment would pin earliest markers onto latest prompts. Shell still stamps `x_grok_turn_idx` from actor state after substitution. | serving: `serving_skips_bands_for_prompt_index`, `prompt_index_mapping_survives_*`; write-back: `writeback_prompt_index_survives_*` |
| **Synthetic vs real user classification** | Typed structure only: `source_messages` emptiness, entry variants, and `message_id` → recorded [`MessageKind`] via one `messages.list` per translation ([`SourceKindIndex`]). **Never** content prefixes or native byte-equality. Band = empty sources; tool / model / thinking = variants; real prompt = sources whose kinds are all `UserPrompt`. **Per-entry** unknown → synthetic; **whole-index** `messages.list` failure → abort (`get_classify_context` `Err` → serve Native / no write-back). **SDK gap:** `SessionThreadView` collapses RuntimeNote → `Message(User)` with the same shape as a prompt (no typed arm); kind is recovered via `message_id`. Serve + write-back share one translator. | `runtime_note_classifies_synthetic_via_message_id_kind`, `runtime_notes_consume_no_prompt_index_slot`, `writeback_live_tail_kinds_round_trip`, `classify_whole_index_failure_fails_open_no_substitution_no_writeback`, `classification_insensitive_to_tool_result_boundary_truncation` |

A substituted request whose host token totals described a different conversation
would be a correctness bug (mysterious truncation later). Serving keeps totals
on the native conversation on purpose; the wire body is authoritative for the
model. **After Replace write-back**, native conversation **is** the LHC body,
so host accounting and the wire view converge (see write-back section below).

## Chunk 2 — compact bridge (hook 5)

`CompactMode::{Off, Shadow, Replace}` — single enum, mutually exclusive writers.
`CompactEventBridge` makes the plan pure and sticky: at most one LHC I/O per
logical event; fail-open never retries.

- `GROK_LHC` off → `Off`
- `GROK_LHC=1` → `Shadow` (native writes; LHC `preview_compact` once at choke)
- `GROK_LHC=1` + `GROK_LHC_COMPACT=replace` + `GROK_LHC_COMPACT_EXPERIMENTAL=1`
  → `Replace` (experimental; otherwise stays Shadow)

### Replace-mode write-back (ruled — not a workaround)

On successful Replace compact, the shell writes LHC's compacted body into
native host state via the existing
`ChatStateHandle::replace_conversation_for_compaction` path (same call native
compaction makes). Native state **becomes** the LHC-compacted state; host
token accounting self-corrects; fail-open to native is safe again because
falling back to native means falling back to the LHC body.

**This is the proven LHC-host architecture** (`pi-lhc`, `t3code`): after a
compact swap the host conversation **is** the rebuilt LHC state.
Serving-without-write-back was the architectural deviation — it created a
two-truths failure mode (LHC view budget vs host request accounting), the
same disease the Hermes integration hit. **Treat any future design where host
and LHC hold divergent conversation state as suspect by default.**

Mechanics (hook 5 choke, independent of hook 4 substitution):

1. `replace_compact_for_writeback` → LHC compact + `get_classify_context`
   (session view + one `messages.list` kind index; same translator as serve)
2. `build_writeback_conversation` — host system prefix + **fixpoint body**
   (see shape below); `prompt_index` from the **tail** of pre-compact native
   markers onto **real users only**
3. Match native `run_compact_inner` surround:
   `record_compaction_at` → **`persist_compaction_checkpoint`** →
   forked-prefix resolution (`resolve_forked_compacted_history` /
   `prefix_released`) → `replace_conversation_for_compaction` → threshold /
   suppress re-check → idle-flush len → memory `context_injected` clear
4. Assert `get_estimated_total_tokens()` decreases (log error if not)

### Write-back body shape (fixpoint — hard gate)

Classification is **typed** from `SessionThreadView` (not text prefixes / not
`LlmRequestContext`). Naively mapping every user-shaped row to
`ConversationItem::user` breaks the fixpoint: `native_prompt_indices` then
counts bands/tools as real turns.

**Chosen shape** (matches native `build_compacted_history` intent):

| SessionThreadView entry | Host item | Notes |
|---|---|---|
| Leading contiguous bands (`User`, empty `source_messages`) | **One** `user_meta` (`CompactionMeta`) | Compressed prefix — prose by design |
| Live-tail `ToolResult` | `ToolResult` | Conserved (`tool_call_id` + content). Host `ToolResultItem` has no `is_error` / `tool_name` fields — omitted, not invented |
| Live-tail `Assistant` text / toolCall | `Assistant` (+ real `tool_calls`) | Conserved |
| Live-tail `Assistant` thinking | `Reasoning` sibling | Conserved via `synthesized_reasoning_item` |
| Live-tail `User` all `UserPrompt` | `User` + tail `prompt_index` | |
| Live-tail `User` RuntimeNote / unknown | `user_meta` (`CompactionMeta`) | Kind via `SourceKindIndex` |
| `Runtime(ModelChange\|ThinkingLevelChange)` | `user_meta` (`CompactionMeta`) | **SDK/host boundary:** no `ConversationItem` variant; view lacks `previous` |
| Host system preamble | Preserved leading `System` | |

**SDK-boundary gap (RuntimeNote):** `tail_entries_of` emits
`RenderingPartKind::RuntimeNote` as `Message(User)` with non-empty
`source_messages` — the same public shape as a real prompt.
`SessionUserMessage` exposes only `content` + `source_messages`. Workaround:
one `messages.list` per translation → [`SourceKindIndex`] keyed by
`message_id` (`MessageRecord.kind`). Cost: one list call alongside the view
fetch (same worker round-trip via `get_classify_context`), not per entry.
Fail toward synthetic on miss. Delete the index path when the SDK adds a
typed RuntimeNote (or Runtime) arm to `SessionThreadView`.

**Tradeoff (structure+text from session view):** Entry counts differ from
`LlmRequestContext` (assistants grouped; no request-context ` · abridged]`
mid-tool marker; model-change text is `provider/model_id` only). That is the
model-facing view LHC intends — serve and write-back share it.

Acceptance:

1. `native_prompt_indices(body)` = live real-user markers only
2. Positional `User(_)` counting for post-compact **rewind** uses the same
   model as native: `needs_compaction_replay` + checkpoint (not
   `truncate_to_prompt_index` alone — that path is wrong after any compact;
   see `rewind.rs`). CompactionMeta items still count as `User(_)` in that
   helper; collapsing bands into one meta keeps the count close to native's
   ~few Users. We evaluated non-`User` (extra `System`) for bands and
   rejected it: `split_system_prefix` would re-accumulate them on the next
   write-back and break the fixpoint.
3. **Fixpoint:** `build_writeback_conversation(body, view) ≡ body` (same
   view, no intervening turns) — tested by `writeback_body_is_fixpoint` /
   `writeback_body_is_fixpoint_through_replace_history`.

Identical-content dedup (a new summary that serializes byte-identically to an
already-recorded item reuses its key) is capture semantics — out of scope;
surfaced to Lee. **Capture tee was not modified.**

### Transient crash window (LHC compact → native replace)

A crash after LHC `compact()` commits but before
`replace_conversation_for_compaction` leaves LHC compacted and native old.
**Transient, not corrupting:** on restart the host bootstraps the old
history; items mint identical keys under stable generation + seeded tracker
and dedup; the next compaction re-derives. Tested by
`writeback_crash_between_lhc_compact_and_native_replace_is_transient`.

### Forked-prefix resolution

Write-back calls the same `resolve_forked_compacted_history` /
`prefix_released` path as native when `inherited_prefix_len` is set. The
threshold/`SUPPRESS_STICKY` branch is therefore not vestigial.

Write-back re-enters capture as `replace_history`. Loop idempotency is a
hard gate — see certification `writeback_*` tests. If that loop is unclean,
fix the *body shape* first; do not unilaterally rewire Chunk 1 capture.

| Concern | Test |
|---|---|
| **Fixpoint** (byte-identical second pass) | `writeback_body_is_fixpoint`, `writeback_body_is_fixpoint_through_replace_history` |
| Prune-shaped post-write-back replace emits nothing | `writeback_prune_shaped_replace_emits_nothing` |
| Genuine band summary records exactly once | `writeback_genuine_compact_summary_records_exactly_once` |
| Repeated unchanged write-back records nothing | `writeback_repeated_unchanged_body_records_nothing` |
| Crash mid-replace → no double on retry | `writeback_crash_mid_replace_no_double_on_retry` |
| Crash LHC-ahead of native replace is transient | `writeback_crash_between_lhc_compact_and_native_replace_is_transient` |
| Estimated tokens decrease after replace | `writeback_replace_decreases_estimated_total_tokens` |
| `prompt_index` survives write-back + rewind/fork | `writeback_prompt_index_survives_*` |
| Rewind after write-back keeps compacted body | `rewind_after_lhc_writeback_shaped_checkpoint_keeps_compacted_body` |

### Chunk 3 live-cert checkpoints (G2)

Fixtures are rebuilt from the realistic post-compact view shape (bands
first, tool results, notes, model-change). A full live harness that runs
shell Replace write-back end-to-end and diffs the delivered body against
fixtures is **mandatory in Chunk 3** if not landed here — register under
Chunk 3 live cert alongside `/btw` + memory-flush on a compacted session.

## Chunk 2 — hook 4 equivalence instrumentation (G1)

Hook 4 stays through Chunk 2 certification as **instrumented-redundant**.
Zero informational divergence through Chunk 3 live cert → remove at Chunk 3
(touchpoint-set change). Any informational divergence → bug or documented
reason to keep the hook; **first instance goes to Lee**.

`observe_serve_equivalence` (shell hook 4, after `apply_serve_decision`)
compares native vs served when **`substituted` is true**. **Observes only —
never changes the served result.** Behind `any_capture_active()` (relaxed atomic) then `capture_active`
(registry) — zero mutex when LHC is off. Disable with
`GROK_LHC_EQUIVALENCE=0|false|off` (default on when the gate is reached).

**Fail-open turns are not evidence.** When serving returns Native, native==served
trivially; those turns increment `turns_fallen_back` only and must not pad the
zero-divergence pile that justifies removing hook 4.

### Counters (`equivalence_snapshot`)

| Field | Meaning |
|---|---|
| `turns_served_and_compared` | Substituted turns that were compared |
| `turns_fallen_back` | Fail-open / Native decisions (not evidence) |
| `structural_divergences` | Compared turns with raw shape mismatch |
| `informational_divergences` | Compared turns with post-projection mismatch (**actionable**) |

### Two signals on compared turns only (do not collapse)

| Signal | Meaning | Expected |
|---|---|---|
| `structural_divergence` | Raw item count / kinds / roles / byte text | Non-zero when the window has tool calls (`LlmRequestContext` has no tool-call representation) |
| `informational_divergence` | After [`project_conversation_canonical`] | **Actionable.** Non-zero is a finding |

Logs **once per session per class** (compared turns only) with session id, turn
index, whether a compact has occurred, counts per side, and the first differing
item.

### Canonical projection — what it normalizes away

`project_conversation_canonical` reduces each side to `(role, canonical_text)`:

- Renders native `Assistant.tool_calls` / `ToolResult` / `Reasoning` into the
  same textual prefixes LHC uses (`[tool call · …]`, `[tool result · …]`,
  `[thinking]…[/thinking]`)
- **Tool-call argument formatting (instrument-only):** parse JSON + compact
  re-serialize with **sorted object keys** ([`canonicalize_tool_arguments`]).
  Cosmetic whitespace and object key order are silent; array element order and
  real value/key changes still fire. Non-JSON passes through. Never applied to
  served/write-back/persisted bodies.
- **Structural fingerprint:** length-framed typed field **projections**
  (`|len:bytes`), plus the unframed `ConversationItem` kind prefix
  (`out.push_str`). Framing is injective over those projections — not over raw
  Rust values before projection. Every `ConversationItem` arm must frame typed
  fields (id, status, sources, summary parts, …), never a rendered aggregate
  like `text_content()` / `text_summary()`. Every contribution that reaches the
  fingerprint string has exactly one of three dispositions (see accounting
  table): **individually pinned**, **jointly pinned**, or
  **documented-redundant**.
- **`push_option_str` vs `push_option_dbg` (W4):** both emit a presence bit
  (`"s"` / `"n"`) then a value field. For `push_option_str`, the bit is
  **load-bearing** — `None` (`|1:n|0:`) vs `Some("")` (`|1:s|0:`) would
  otherwise collide, and forcing the `None` arm to emit `"s"` fails the
  `pin_option_*_absent_vs_empty` suite; a payload starting with `s`/`n` cannot
  migrate into the presence slot (length-framed). For `push_option_dbg`, the
  bit is **defence-in-depth and inert** given non-empty `Debug` output: `None`
  (`|1:n|0:`) vs `Some(v)` (`|1:s|N:…`) still differ on the value field even if
  the `None` arm is forced to `"s"`. Do **not** add absent-vs-present pins for
  `User.cwd_generation`, `prior_turn_interrupt`, or `prompt_index` to “fix”
  that inert bit. Value-only pins for those Options remain.
- **Count fields (W3) — documented-redundant, do not pin, do not remove:**
  `User.content_count`, `AsstTools.toolcalls_count`, `ToolResult.images_count`,
  `WS.sources_count` (including the `"0"` literal on the `None` sources arm),
  `Reasoning.summary_count`, `Reasoning.content_count` (including `"0"` on the
  `None` content arm). Counts are **derived** from the vector they precede, so
  “two items differing only in the count” describes no constructible pair — no
  test can pin them in principle. Deleting a count leaves the encoding
  **injective anyway**: `push_framed` is length-prefixed so the field sequence
  is uniquely decodable, and each arm has fixed arity around its loop
  (`ToolResult` = 2 fixed + 2·k; `AsstTools` = 6 fixed + 3·k + trailing content;
  `Reasoning`’s summary terminates on the literal `"summary_text"` tag, which
  can never equal the `"n"`/`"s"` presence bit that follows — noted at the code
  site). Their weaker presence creates **no false-negative risk**.
- Drops `synthetic_reason`, `prompt_index`, and model metadata from the
  comparison (role + text only)
- Line endings (`\r\n`/`\r` → `\n`) and outer trim only (`normalize_whitespace`)
  — **internal spaces, tabs, indentation, and blank lines are preserved**
  outside tool-argument JSON
- Contiguous `[context · …` band items collapsed to one (`\n\n` join), matching
  write-back’s single CompactionMeta vs serving’s N items

**Does not** normalize away non-band item count or message content (array
element order stays significant; object key order is normalized away above).

Prune/mutation serving is **out of Phase 3 scope**. If a later phase adds it,
mutations route through the **native replacement path** (write-back law), never
a revived serving substitution.

### Fingerprint contribution accounting (70 deletion units)

A deletion unit is anything that reaches the fingerprint string: one
`push_framed(…)` call site, one `push_option_*` call (presence + value
together), or one `out.push_str(kind)` site. Three dispositions:

| Disposition | Meaning |
|---|---|
| **individually pinned** | A named test fails when **that contribution alone** is removed from the encoder. |
| **jointly pinned** | Named pair `(A, B)`: deleting **either alone** leaves some fixture pair distinguishable (suite may stay green); deleting **both together** fails the named joint test. The pair is the load-bearing unit — do not reshape the encoder to force an individual pin. |
| **documented-redundant** | No constructible individual pin; injectivity holds without the contribution (argument recorded). Deleting it alone leaves the suite green by design. |

| # | Contribution | Disposition | Test / argument |
|---|---|---|---|
| 1 | `System.content` | individually pinned | `pin_system_content` |
| 2 | `User.synthetic_reason` (`push_option_dbg`) | individually pinned | `pin_user_synthetic_reason` (value); presence bit inert — W4 |
| 3 | `User.cwd_generation` (`push_option_dbg`) | individually pinned | `pin_user_cwd_generation` (value); presence bit inert — W4 |
| 4 | `User.prior_turn_interrupt` (`push_option_dbg`) | individually pinned | `pin_user_prior_turn_interrupt` (value); presence bit inert — W4 |
| 5 | `User.prompt_index` (`push_option_dbg`) | individually pinned | `pin_user_prompt_index` (value); presence bit inert — W4 |
| 6 | `User.content_count` | documented-redundant | W3 derived-count argument |
| 7 | `ContentPart.Text` tag `"t"` | individually pinned | `pin_content_part_text_tag` (payload ≠ `"t"`) |
| 8 | `ContentPart.Text.text` | individually pinned | `pin_content_part_text_payload` |
| 9 | `ContentPart.Image` tag `"i"` | individually pinned | `pin_content_part_image_tag` (url ≠ `"i"`) |
| 10 | `ContentPart.Image.url` | individually pinned | `pin_content_part_image_url` |
| 11 | `Assistant.model_id` (`push_option_str`) | individually pinned | `pin_assistant_model_id` + `pin_option_assistant_model_id_absent_vs_empty` |
| 12 | `Assistant.model_fingerprint` (`push_option_str`) | individually pinned | `pin_assistant_model_fingerprint` + absent_vs_empty |
| 13 | `Assistant.reasoning_effort` (`push_option_dbg`) | individually pinned | `pin_assistant_reasoning_effort` (value); presence bit inert — W4 |
| 14 | `AsstTools.toolcalls_count` | documented-redundant | W3 derived-count argument |
| 15 | `AsstTools.tc.name` | individually pinned | `pin_assistant_tools_tc_name` |
| 16 | `AsstTools.tc.id` | individually pinned | `pin_assistant_tools_tc_id` |
| 17 | `AsstTools.tc.arguments` | individually pinned | `pin_assistant_tools_tc_arguments` |
| 18 | `AsstTools.content` | individually pinned | `pin_assistant_tools_content` |
| 19 | `Assistant.content` | individually pinned | `pin_assistant_content` |
| 20 | `ToolResult.tool_call_id` | individually pinned | `pin_tool_result_tool_call_id` |
| 21 | `ToolResult.content` | individually pinned | `pin_tool_result_content` |
| 22 | `ToolResult.images_count` | documented-redundant | W3 derived-count argument |
| 23 | `WS` kind tag `"web_search"` | individually pinned | `pin_tag_web_search` (id/query ≠ tag) |
| 24 | `WS.id` | individually pinned | `raw_fingerprint_backend_tool_distinct_ids_do_not_collide` |
| 25 | `WS.status` | individually pinned | `raw_fingerprint_backend_tool_status_difference_registers` |
| 26 | `WS.Search` action tag `"search"` | individually pinned | `pin_tag_search_action` (query ≠ `"search"`) |
| 27 | `WS.Search.query` | individually pinned | `pin_websearch_search_query` |
| 28 | `WS.Search.sources` presence `"n"` (None arm) | jointly pinned | pair with #30; `pin_joint_websearch_sources_presence_pair` |
| 29 | `WS.sources_count` `"0"` (None arm) | documented-redundant | W3 derived-count argument |
| 30 | `WS.Search.sources` presence `"s"` (Some arm) | jointly pinned | pair with #28; `pin_joint_websearch_sources_presence_pair` |
| 31 | `WS.sources_count` (Some arm len) | documented-redundant | W3 derived-count argument |
| 32 | `WS.Search.src.type` | individually pinned | `pin_websearch_source_type` |
| 33 | `WS.Search.src.url` | individually pinned | `pin_websearch_source_url` |
| 34 | `WS.OpenPage` action tag `"open_page"` | individually pinned | `pin_tag_open_page_action` |
| 35 | `WS.OpenPage.url` (`push_option_str`) | individually pinned | `pin_option_openpage_url_absent_vs_empty` + `pin_websearch_open_page_url_value` |
| 36 | `WS.Find` action tag `"find"` | individually pinned | `pin_tag_find_action` (url/pattern ≠ `"find"`) |
| 37 | `WS.Find.url` | individually pinned | `pin_websearch_find_url` |
| 38 | `WS.Find.pattern` | individually pinned | `pin_websearch_find_pattern` |
| 39 | `WS.FindInPage` action tag `"find_in_page"` | individually pinned | `pin_tag_find_in_page_action` |
| 40 | `WS.FindInPage.url` | individually pinned | `pin_websearch_find_in_page_url` |
| 41 | `WS.FindInPage.pattern` | individually pinned | `pin_websearch_find_in_page_pattern` |
| 42 | `XSearch` kind tag `"x_search"` | individually pinned | `pin_tag_x_search` (`name` ≠ `"x_search"` — X1) |
| 43 | `XSearch.id` | individually pinned | `pin_xsearch_id` |
| 44 | `XSearch.call_id` | individually pinned | `pin_xsearch_call_id` |
| 45 | `XSearch.name` | individually pinned | `pin_xsearch_name` |
| 46 | `XSearch.input` | individually pinned | `pin_xsearch_input` |
| 47 | `CodeInterp` kind tag `"code_interpreter"` | individually pinned | `pin_tag_code_interpreter` |
| 48 | `CodeInterp.id` | individually pinned | `pin_codeinterp_id` |
| 49 | `CodeInterp.status` | individually pinned | `pin_codeinterp_status` |
| 50 | `CodeInterp.container_id` | individually pinned | `pin_codeinterp_container_id` |
| 51 | `CodeInterp.code` (`push_option_str`) | individually pinned | `pin_option_codeinterp_code_absent_vs_empty` + `pin_codeinterp_code_value` |
| 52 | `CodeInterp.outputs` | individually pinned | `pin_option_codeinterp_outputs_null_vs_empty_vec` |
| 53 | `Reasoning.id` | individually pinned | `pin_reasoning_id` |
| 54 | `Reasoning.status` (`push_option_dbg`) | individually pinned | `pin_reasoning_status` (value); presence bit inert — W4 |
| 55 | `Reasoning.encrypted` (`push_option_str`) | individually pinned | `pin_option_reasoning_encrypted_absent_vs_empty` + encrypted_only_differs |
| 56 | `Reasoning.summary_count` | documented-redundant | W3 derived-count argument |
| 57 | `Reasoning.summary` tag `"summary_text"` | individually pinned | `pin_reasoning_summary_tag` (text ≠ `"summary_text"`) |
| 58 | `Reasoning.summary.text` | individually pinned | `pin_reasoning_summary_text` / `raw_fingerprint_reasoning_summary_parts_do_not_collide` |
| 59 | `Reasoning.content` presence `"n"` (None arm) | jointly pinned | pair with #61; `pin_joint_reasoning_content_presence_pair` |
| 60 | `Reasoning.content_count` `"0"` (None arm) | documented-redundant | W3 derived-count argument |
| 61 | `Reasoning.content` presence `"s"` (Some arm) | jointly pinned | pair with #59; `pin_joint_reasoning_content_presence_pair` |
| 62 | `Reasoning.content_count` (Some arm len) | documented-redundant | W3 derived-count argument |
| 63 | `Reasoning.content.text` | individually pinned | `pin_reasoning_content_text` |
| 64 | kind `"system"` (`push_str`) | individually pinned | `pin_kind_system` |
| 65 | kind `"user"` / `"user_meta"` (`push_str`) | individually pinned | `pin_kind_user` + `pin_kind_user_meta` |
| 66 | kind `"assistant_tools"` (`push_str`) | individually pinned | `pin_kind_assistant_tools` |
| 67 | kind `"assistant"` (`push_str`) | individually pinned | `pin_kind_assistant` (prefix `"assistant\|"`, not `"assistant"`) |
| 68 | kind `"tool_result"` (`push_str`) | individually pinned | `pin_kind_tool_result` |
| 69 | kind `"backend_tool_call"` (`push_str`) | individually pinned | `pin_kind_backend_tool_call` |
| 70 | kind `"reasoning"` (`push_str`) | individually pinned | `pin_kind_reasoning` |

| Test | Expect |
|---|---|
| `equiv_text_only_window_both_silent` | substituted: structural = 0, informational = 0, compared += 1 |
| `equiv_tool_window_structural_only` | substituted: structural ≠ 0, informational = 0 when projection matches |
| `equiv_tool_arg_cosmetic_formatting_silent_different_paths` | native pretty vs `decide_substitution` translator: both silent |
| `equiv_tool_arg_real_change_informational_different_paths` | native vs translator real arg change: informational ≠ 0 |
| `equiv_swapped_tool_call_registers_structurally` | native vs translator swapped tool: structural ≠ 0 |
| `equiv_tool_arg_object_key_reorder_silent` | object key reorder: both channels silent |
| `equiv_tool_arg_array_reorder_divergent` | array element reorder: informational ≠ 0 |
| `raw_fingerprint_*_is_injective_across_delimiter_collision` | length-framed fingerprint distinguishes old collision pairs |
| `raw_fingerprint_reasoning_summary_parts_do_not_collide` | same id + same count; summary boundary only-diff ⇒ distinct (W2) |
| `pin_*` / `pin_tag_*` / `pin_kind_*` / `pin_option_*` | see 70-row accounting — individually pinned contributions |
| `pin_joint_websearch_sources_presence_pair` | jointly pinned #28+#30; BWR deletes both presence arms |
| `pin_joint_reasoning_content_presence_pair` | jointly pinned #59+#61; BWR deletes both presence arms |
| `pin_websearch_find_url` / `pin_websearch_find_pattern` | Find payload only-diffs (W2; not variant tags) |
| `equiv_post_writeback_band_collapse_informational_silent` | write-back vs serve bands: structural ≠ 0, informational = 0 |
| `equiv_fail_open_turn_not_counted_as_compared` | fail-open: compared unchanged, fallback += 1, neither divergence counter moves |

## Chunk 2 — inference sampler (hook 2 widen)

`Arc<dyn LhcInferenceSampler>` is supplied at `tee_chat_persistence` /
`spawn_capture`. Trait in `grok-lhc-host`; shell implements
`ShellLhcInferenceSampler`. Adapter tests use `MockLhcInferenceSampler`.
