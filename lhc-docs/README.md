# grok-build-lhc — what this fork is

A maintained fork of Grok Build that aims at **long-horizon work**: sessions
that stay useful across many hours or days, not just until the first context
crunch.

If you only need a short orientation: **this is Grok Build with better memory
for long projects.** The rest of this page is the why and how, still short.
Install steps live in [Install & use](INSTALL.md). Maintainer contract:
[`FORK.md`](../FORK.md).

---

## The problem

Coding agents are strong in the moment and weak over time.

As a session grows, the window fills. The usual fix is to **summarize or drop
older context**. Do that repeatedly and you get a cliff: crisp recent turns,
a vague mush further back, and no honest way back to the real trail of
decisions, failures, and constraints.

That is acceptable for a one-hour task. It is a bad foundation when you are
steering a real codebase for a week — the agent “forgets” in the worst way:
not gently, but by destroying the only high-fidelity copy of what happened.

## The purpose of this fork

Keep **Grok Build** (SpaceXAI’s coding agent harness) as the thing you drive
day to day, and give it a **context layer built for long horizons**:

- Stay coherent longer without stuffing the window with every tool dump forever  
- Prefer a **gentle fade** of older work over a single hard cliff  
- Keep a path back to detail when the thin view is not enough  
- Remain **opt-in**, so the fork is still usable as plain Grok Build  

The engine behind that is
[**LHC** (Long Horizon Context)](https://github.com/liminal-ai/long-horizon-context)
— the same project used on other hosts (for example
[codex-lhc](https://github.com/liminal-ai/codex-lhc)). This repository is the
**Grok Build host**: LHC integrated into this codebase and kept merging with
upstream.

## How it thinks about memory (high level)

**Two layers, not one:**

1. **What actually happened** — the full session trail is retained so it can
   be rebuilt and inspected later.  
2. **What the model sees right now** — a working view sized for the window:
   recent material sharp, older material more compressed.

Compaction is closer to **re-rendering a long story at a target length** than
to “replace the past with one irreversible paragraph.” When a compressed
stretch is not enough, the agent can **ask for specific past turns or
messages** and refresh that slice — history-native retrieval, not “hope
search finds it.”

If you want the deep design (bands, derivations, intake, ethics of not
erasing archives), read the LHC repo’s
[onboard docs](https://github.com/liminal-ai/long-horizon-context/tree/main/docs/onboard).
You do not need that to evaluate whether this fork is interesting.

## What you get in Grok, practically

| Capability | In plain terms |
|---|---|
| Capture | Session activity can be recorded into the long-horizon store |
| Banded context | Model context prefers a ramp of fidelity over a single cliff |
| Compact | LHC can own compaction instead of only native auto-compact |
| Pull | Tools to re-open older turns/messages when needed |
| Operator controls | `/lhc` status, health, on/off, repair |

Default is **off**. Enable when you want the long-horizon path; leave it off
and you are close to stock Grok Build behavior.

## What this fork is not

- Not an official xAI product line or release channel  
- Not a ground-up rewrite of Grok Build  
- Not “always on” memory theater — it is gated and still evolving  
- Not a substitute for reading upstream Grok docs for the base CLI/TUI  

## Branches, releases, maintenance

| | |
|---|---|
| **`lhc`** (default) | Product branch: Grok + LHC |
| **`main`** | Upstream mirror only |
| **Releases** | No prebuilt fork binaries yet — build from source ([Install](INSTALL.md)). Fork releases may come later. |
| **Upstream** | Merged regularly; the fork is designed to survive that churn |

## Where to go next

| You want… | Go to |
|---|---|
| Build, enable, verify | [Install & use](INSTALL.md) |
| How the integration is structured (hooks, layout) | [Technical shape](#technical-shape-optional) below, then [`FORK.md`](../FORK.md) |
| LHC concepts in depth | [long-horizon-context](https://github.com/liminal-ai/long-horizon-context) · [docs/onboard](https://github.com/liminal-ai/long-horizon-context/tree/main/docs/onboard) |
| Base Grok product | Root [README](../README.md) below the banner · [x.ai/cli](https://x.ai/cli) |

---

## Technical shape (optional)

Only if you are already sold on the purpose and want the map.

```
crates/lhc/
  vendor/long-horizon-context/   LHC SDK (submodule, pinned)
  grok-lhc-host/                 adapter (capture, serve, compact, tools)
```

Core Grok touchpoints stay small and marked (`LHC-HOOK`); almost all LHC
logic lives in the adapter so upstream can move without a rewrite. Details,
sync drill, and recovery: [`FORK.md`](../FORK.md).

**Status (honest):** capture, serve, banded compact, and history pull are
integrated and gated. Live certification and some compact modes are still
active work. Prefer [Install](INSTALL.md) for “can I run it?” and `FORK.md`
for “can I maintain it?”
