#!/usr/bin/env python3
"""Daytona Linux smoke for grok-build-lhc release artifacts.

Requires:
  DAYTONA_API_KEY

Inputs (env):
  GROK_ARTIFACT_PATH  — local path to grok-*-linux-x86_64 binary
  or GROK_ARTIFACT_URL — https URL to download the binary

Checks (phase 1 basics):
  1. Create ephemeral Linux sandbox (daytona-medium)
  2. Upload binary as ~/grok, chmod +x
  3. Dynamic linker / shared libs resolve (ldd)
  4. `grok --version` exits 0 and prints a version
  5. `grok --help` exits 0
  6. Optional: GROK_LHC=0 still runs --version (gate doesn't crash binary)

Does not yet: full TUI, /lhc interactive, long-horizon session e2e.
"""

from __future__ import annotations

import os
import sys
import tempfile
import urllib.request
from pathlib import Path

from daytona import CreateSandboxFromSnapshotParams, Daytona, FileUpload


def fail(msg: str, code: int = 1) -> None:
    print(f"FAIL: {msg}", file=sys.stderr)
    raise SystemExit(code)


def ok(msg: str) -> None:
    print(f"OK: {msg}")


def resolve_artifact() -> Path:
    path = os.environ.get("GROK_ARTIFACT_PATH")
    url = os.environ.get("GROK_ARTIFACT_URL")
    if path:
        p = Path(path)
        if not p.is_file():
            fail(f"GROK_ARTIFACT_PATH not a file: {p}")
        return p
    if url:
        dest = Path(tempfile.gettempdir()) / "grok-linux-smoke-artifact"
        print(f"Downloading {url} -> {dest}")
        urllib.request.urlretrieve(url, dest)
        dest.chmod(0o755)
        return dest
    fail("Set GROK_ARTIFACT_PATH or GROK_ARTIFACT_URL")


def require_key() -> None:
    if not os.environ.get("DAYTONA_API_KEY"):
        fail("DAYTONA_API_KEY is not set")


def main() -> None:
    require_key()
    artifact = resolve_artifact()
    size = artifact.stat().st_size
    if size < 1_000_000:
        fail(f"artifact looks too small ({size} bytes): {artifact}")
    ok(f"artifact {artifact} ({size} bytes)")

    d = Daytona()
    sb = d.create(
        CreateSandboxFromSnapshotParams(
            snapshot=os.environ.get("DAYTONA_SNAPSHOT", "daytona-medium"),
            labels={
                "purpose": "grok-lhc-smoke",
                "platform": "linux",
                "project": "grok-build-lhc",
            },
            auto_stop_interval=15,
            ephemeral=True,
        ),
        timeout=float(os.environ.get("DAYTONA_CREATE_TIMEOUT", "180")),
    )
    ok(f"sandbox {sb.id} state={sb.state}")

    remote = "/home/daytona/grok"
    try:
        sb.fs.upload_files(
            [FileUpload(source=str(artifact), destination=remote)]
        )
        ok(f"uploaded -> {remote}")

        r = sb.process.exec(f"chmod +x {remote} && file {remote} && ldd {remote} 2>&1 | head -30")
        print(r.result or "")
        if r.exit_code != 0:
            fail(f"chmod/file/ldd failed: {r.exit_code}")
        if "not a dynamic executable" in (r.result or "") and "ELF" not in (r.result or ""):
            fail("unexpected file type")
        if "not found" in (r.result or "") and "lib" in (r.result or ""):
            # ldd missing libs often says " => not found"
            if "=> not found" in (r.result or ""):
                fail(f"missing shared libraries:\n{r.result}")
        ok("binary looks dynamically loadable")

        r = sb.process.exec(
            f"{remote} --version 2>&1 || {remote} -V 2>&1",
            timeout=60,
        )
        print(r.result or "")
        if r.exit_code != 0:
            fail(f"--version failed: {r.exit_code}")
        out = (r.result or "").strip()
        if "grok" not in out.lower() and not any(c.isdigit() for c in out):
            fail(f"version output unexpected: {out!r}")
        ok(f"version: {out.splitlines()[0]}")

        r = sb.process.exec(f"{remote} --help 2>&1 | head -50", timeout=60)
        print(r.result or "")
        if r.exit_code != 0:
            fail(f"--help failed: {r.exit_code}")
        if "Usage:" not in (r.result or "") and "usage" not in (r.result or "").lower():
            fail("--help missing Usage")
        ok("help ok")

        r = sb.process.exec(
            f"GROK_LHC=0 {remote} --version 2>&1",
            timeout=60,
        )
        if r.exit_code != 0:
            fail(f"version with GROK_LHC=0 failed: {r.exit_code}")
        ok("version with GROK_LHC=0 ok")

        print("SMOKE_PASS linux")
    finally:
        try:
            d.delete(sb)
            ok("sandbox deleted")
        except Exception as e:
            print(f"WARN: delete sandbox: {e}", file=sys.stderr)


if __name__ == "__main__":
    main()
