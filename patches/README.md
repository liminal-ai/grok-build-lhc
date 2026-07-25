# LHC hook patch series

Every core-file touchpoint (an `LHC-HOOK` marked insertion outside
`crates/lhc/`) is maintained BOTH as normal commits on the `lhc` branch AND
as a `git format-patch` file here, regenerated whenever a hook changes.

On a normal upstream sync these files are redundant. They exist for the day
upstream resets history (this repo is a daily monorepo squash-sync; ancestry
is not guaranteed): fresh clone of new upstream -> re-add `crates/lhc/` ->
`git am patches/*.patch` -> `scripts/check-lhc-hooks.sh`. The full drill is
in /FORK.md.

Series (Chunk 1 — **patches pending regeneration** after the orchestrator
commits; run `git format-patch` per FORK.md / this note):
- 0001: root `Cargo.toml` workspace-members entry for
  `crates/lhc/grok-lhc-host` + shell `Cargo.toml` dependency hook (1/3)
- 0002: persistence-tee hook in `spawn.rs` (2/3)
- 0003: model/thinking-level change tee in `model_switch.rs` (3/3)
