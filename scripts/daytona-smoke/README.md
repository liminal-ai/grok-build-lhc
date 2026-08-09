# Daytona smoke tests (grok-build-lhc)

Install release/dry-run **artifacts** into Daytona sandboxes and run basics.

## Auth

```bash
export DAYTONA_API_KEY=...   # restricted smoke key preferred
```

Admin keys are for setup only; rotate to a sandbox-scoped key for CI.

GitHub Actions secret (already used by workflow): `DAYTONA_API_KEY`.

## Linux (primary)

```bash
python3 -m venv .venv && . .venv/bin/activate
pip install -r scripts/daytona-smoke/requirements.txt

# from a local artifact (e.g. downloaded from Actions dry-run):
export GROK_ARTIFACT_PATH=./grok-0.1.0-linux-x86_64
python scripts/daytona-smoke/smoke_linux.py
```

Exit `0` = pass. Prints `SMOKE_PASS linux`.

## Windows

Requires org access to **Windows sandboxes** (not available on Daytona Tier 1/2
by default — contact Daytona support). When blocked, script exits `2` (skip).

```bash
export GROK_ARTIFACT_PATH=./grok-0.1.0-windows-x86_64.exe
python scripts/daytona-smoke/smoke_windows.py
```

## Phase 1 checks

1. Create ephemeral sandbox  
2. Upload artifact as `grok`  
3. Shared libs / file type  
4. `--version`  
5. `--help`  
6. (Linux) still runs with `GROK_LHC=0`

## Later (not yet)

Long-horizon session: install → run headless turns → assert LHC storage /
retrieval. Keep that behind the basics staying green.
