# grok-build-lhc — LHC context management for Grok Build

Fork of [`xai-org/grok-build`](https://github.com/xai-org/grok-build) adding
[LHC](https://github.com/liminal-ai/long-horizon-context) (Long Horizon
Context): event-sourced capture of every session into a per-thread SQLite
record, with banded compaction replacing native auto-compact — full history
preserved and rebuildable at full fidelity. Working branch and default
branch: `lhc`. `main` tracks upstream, untouched.

Status: Chunk 0 (fork scaffolding) of the 3-chunk integration
(`long-horizon-context/docs/lhc-rs-port/phase3-grok-build-integration-brief.md`).
Nothing runs yet; no core file is modified yet.

## Layout

- `crates/lhc/grok-lhc-host/` — the adapter crate (capture mapping, compact
  bridge, ModelCall). Fork-only; standalone workspace during Chunk 0.
- `crates/lhc/vendor/long-horizon-context/` — submodule, pinned to certified
  commits of the `lhc-rs-port` branch only (Phase 2 acceptance `358c8d1` or
  later). Never copy the port in; bump the pin and record it here.
  Current pin: `e582465`.
- `patches/` — every core touchpoint as a re-appliable `format-patch` file
  (see `patches/README.md`). The history-reset recovery path.
- `scripts/check-lhc-hooks.sh` — the three-layer tripwire (sentinel count,
  compile, golden smoke). Run after every sync, before every push.

## Touchpoint inventory (every owned core line)

| # | File | Lines | Purpose | Patch |
|---|------|-------|---------|-------|
| — | none yet | — | Chunk 0 makes zero core touches | — |

Rules: hooks are 1–5 line additive insertions marked
`// LHC-HOOK <n>/<total>: <purpose>`; the sentinel total in
`scripts/check-lhc-hooks.sh` and this table change in the same commit as any
hook; each hook is regenerated into `patches/` in that same commit.

## Upstream model (read before your first sync)

Upstream publishes via daily monorepo squash-syncs — every commit is
"Synced from monorepo" (6.7k–57k line diffs), external PRs are not accepted,
and **history may be rewritten or reset without notice**. Ancestry is a
convenience here, not a guarantee: the patch series + this file are the
durable representation of the fork.

## Sync drill (weekly or on-need, not per upstream commit)

1. `git fetch upstream && git checkout lhc && git merge upstream/main`
2. Expected recurring conflict: root `Cargo.toml` is auto-generated and
   sorted; from Chunk 1 on, our `crates/lhc/grok-lhc-host` members entry
   (patch 0001) will collide. Resolution rule: take upstream's list, re-add
   our single entry in sort order. Never hand-resolve anything else in that
   file.
3. `scripts/check-lhc-hooks.sh` — all three layers green (or golden layer
   SKIP before Chunk 1 lands).
4. Fast-forward `main` to `upstream/main`, push both branches.
5. Sync commit body records: upstream range, tripwire results, smoke verdict.

## History-reset recovery drill

If `git merge upstream/main` reports unrelated histories or the diff is
implausibly large, upstream reset. Do not merge. Instead:

1. Fresh clone of new upstream; branch `lhc` from its tip.
2. Copy `crates/lhc/` (or re-add the submodule + adapter), `patches/`,
   `scripts/check-lhc-hooks.sh`, `FORK.md` from the old tree — these are
   fork-owned, upstream never touches them.
3. `git am patches/*.patch` for every core touchpoint.
4. `scripts/check-lhc-hooks.sh` — green means the fork is whole.
5. Force-update `origin/lhc`; keep the old tree until green.

Rehearse this once before Chunk 3 sign-off (brief, Chunk 0 certification).

## Never run

- `grok upgrade` / any self-update: this is a source checkout; self-update
  would clobber the tree.
- `git merge` after a suspected history reset (see drill above).

## Host obligations toward LHC (from the port's acceptance record)

- Timestamps passed into LHC public APIs must be canonical
  `YYYY-MM-DDTHH:MM:SS(.mmm)Z` (Amendment D ceiling).
- Do not set `SdkConfig.clock` in production (cross-port provenance parity).
