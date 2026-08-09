# grok-build-lhc — what this fork is

A maintained public fork of
[`xai-org/grok-build`](https://github.com/xai-org/grok-build) that adds
[**LHC** (Long Horizon Context)](https://github.com/liminal-ai/long-horizon-context).

This page is the short tour: what problem we care about, what LHC does, and
how that shows up inside Grok Build. For build and enable steps, see
[Install & use](INSTALL.md). For hooks, sync drills, and recovery (maintainers),
see [`FORK.md`](../FORK.md).

---

## The problem in one minute

Coding agents fill a context window. When it gets full, most harnesses
**summarize the old part and drop the original**. Do that a few times and the
session develops a cliff: sharp recent work, a mush of summaries further
back, and no honest way back to what actually happened.

That is fine for a short task. It is a bad foundation for days or weeks of
continuous work.

## What LHC does instead

LHC keeps a **full durable record** of the session (typed events in
per-thread SQLite). What the model *sees* is a **rendering** of that record:

| Closer to now | Further back |
|---|---|
| Full fidelity (live tail) | Smoother turn-level texture |
| | Then fuller summaries |
| | Then brief outcomes |

So memory **ramps down** instead of falling off a cliff. The originals stay
in the archive. Compaction assembles a new view from material already derived;
it does not invent a one-time irreversible summary blob as the only truth.

When a thin band is not enough, the agent can **pull** specific past turns or
messages at higher fidelity (`get_turns` / `get_messages`) instead of hoping a
keyword search guesses right.

Full concepts and design live in the LHC repo — start at
[`docs/onboard/`](https://github.com/liminal-ai/long-horizon-context/tree/main/docs/onboard)
if you want depth.

## What this fork is (and is not)

| This fork is | This fork is not |
|---|---|
| Grok Build **plus** LHC capture, serve, compact, and retrieval | A rebrand of Grok or a replacement for xAI’s product |
| A **maintained** integration branch (`lhc`) rebased/merged with upstream | A one-off patch dump |
| **Opt-in** — off by default; flag-off ≈ upstream behavior | Forced on for every user of `grok` |
| Built from **source** today | An official xAI release channel |

**Branches**

- **`lhc`** (default) — fork product: LHC host + core touchpoints + docs
- **`main`** — upstream mirror only (no LHC)

**Releases**

There are no prebuilt fork releases yet. Install and run from this tree. We
expect to publish fork binaries later so day-to-day use does not require a
local build; until then, [Install & use](INSTALL.md) is the path.

## How it sits in Grok Build

Integration is intentionally thin so upstream can move daily:

```
crates/lhc/
  vendor/long-horizon-context/   LHC SDK (git submodule, pinned)
  grok-lhc-host/                 adapter — mapping, capture, serve, compact
```

Core Grok files only get small, marked call sites (`LHC-HOOK`); almost all
logic lives in the adapter. Inventory and laws: [`FORK.md`](../FORK.md).

In practice that means:

1. **Capture** — conversation items are teed into LHC as the session runs  
2. **Serve** — when capture is active, model requests can use the LHC view  
3. **Compact** — LHC smart compact can replace native auto-compact (shadow or
   experimental replace modes)  
4. **Retrieve** — tools to pull historical turns/messages from the archive  
5. **Operator surface** — `/lhc` status, health, repair, per-session on/off  

## Related projects

LHC is one engine with several hosts. This repo is the **Grok Build** host.

- [**long-horizon-context**](https://github.com/liminal-ai/long-horizon-context) — SDK, onboard docs, other host packages  
- [**codex-lhc**](https://github.com/liminal-ai/codex-lhc) — same idea on OpenAI Codex  

## Status (honest, short)

- Capture / serve / banded compact / history pull are integrated and gated.  
- Default remains **off** until you enable it.  
- Live certification and some experimental compact paths are still active
  maintainer work — see `FORK.md` and
  `crates/lhc/grok-lhc-host/LIVE_RUNBOOK.md` if you are deep in the weeds.  
- Upstream Grok Build is a monorepo sync; this fork is designed to **merge
  upstream regularly** and survive history resets via patches + tripwires.

## Where to go next

| You want… | Go to |
|---|---|
| Build, enable, verify | [Install & use](INSTALL.md) |
| Maintainer contract (hooks, sync, recovery) | [`FORK.md`](../FORK.md) |
| LHC itself (concepts, other hosts) | [liminal-ai/long-horizon-context](https://github.com/liminal-ai/long-horizon-context) |
| Deeper LHC design reading | [docs/onboard/](https://github.com/liminal-ai/long-horizon-context/tree/main/docs/onboard) |
| Upstream Grok product docs | Remainder of the root [README](../README.md) and [x.ai/cli](https://x.ai/cli) |
