# grok-build-lhc — what this fork is

**Grok Build + LHC** is a maintained fork of
[`xai-org/grok-build`](https://github.com/xai-org/grok-build) with better
**long-horizon context management**.

It keeps a **full transcript** of the session and serves **long-horizon
views**: older history is progressively compressed (lower fidelity further
back), the latest work stays at full fidelity, and the transition between
them is a smooth ramp—not a single cliff where everything older becomes one
lossy paragraph. The goal is agents that stay **coherent and crisp** across
histories on the order of **tens of millions of tokens**, not only until the
first window fills. Compressed content remains **tagged for high-fidelity
retrieval** when Grok needs a deeper dive into something the thin view only
sketches.

That engine is
[**LHC** (Long Horizon Context)](https://github.com/liminal-ai/long-horizon-context).
This repo is the **Grok Build host**—same idea as other LHC hosts (e.g.
[codex-lhc](https://github.com/liminal-ai/codex-lhc)).

Install: [Install & use](INSTALL.md). Maintainer contract: [`FORK.md`](../FORK.md).

---

## Why that matters (short)

Usual agent memory: window fills → summarize or drop the past → repeat. Detail
and “why we did that” erode fast. Fine for a short task; weak for multi-day
work on a real codebase.

This fork’s bet: **retain the trail**, **show a ramp**, **pull detail on
demand**. Plausible if you already feel the cliff; skippable if you only run
short sessions.

Deeper design (bands, archive vs view, ethics of not erasing history): LHC
[onboard docs](https://github.com/liminal-ai/long-horizon-context/tree/main/docs/onboard).

## What you get in Grok, practically

| | |
|---|---|
| Full transcript | Session trail retained under the working view |
| Long-horizon views | Older → more compressed; recent → full fidelity; smooth ramp |
| Scale ambition | Coherence aimed at very large histories (tens of millions of tokens of record), with a target-sized working context |
| Tagged retrieval | Re-open specific past turns/messages when the thin view is not enough |
| Default | **On** when you run this fork; disable only to troubleshoot |
| Controls | `/lhc` status, health, on/off, repair |

## What this fork is not

- Not an official xAI release channel  
- Not a rewrite of Grok Build—thin integration, regular upstream merges  
- Not a substitute for stock Grok when comparing behavior—use upstream builds for that  


## Branches and releases

| | |
|---|---|
| **`lhc`** (default) | Product: Grok + LHC |
| **`main`** | Upstream mirror only |
| **Releases** | No prebuilt fork binaries yet — [build from source](INSTALL.md). Fork releases may come later. |

## Where to go next

| You want… | Go to |
|---|---|
| Build, enable, verify | [Install & use](INSTALL.md) |
| Integration map / sync / hooks | [Technical shape](#technical-shape-optional), then [`FORK.md`](../FORK.md) |
| LHC in depth | [long-horizon-context](https://github.com/liminal-ai/long-horizon-context) · [docs/onboard](https://github.com/liminal-ai/long-horizon-context/tree/main/docs/onboard) |
| Base Grok product | Root [README](../README.md) (below the banner) · [x.ai/cli](https://x.ai/cli) |

---

## Technical shape (optional)

```
crates/lhc/
  vendor/long-horizon-context/   LHC SDK (submodule, pinned)
  grok-lhc-host/                 adapter (capture, serve, compact, tools)
```

Core touchpoints stay small and marked (`LHC-HOOK`); most logic is in the
adapter. Details: [`FORK.md`](../FORK.md).

**Status:** capture, serve, banded views, and history pull are integrated and
gated. Live certification and some compact modes are still active work.
