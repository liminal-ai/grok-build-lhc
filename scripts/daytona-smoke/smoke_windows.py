#!/usr/bin/env python3
"""Daytona Windows smoke for grok-build-lhc (when org has Windows sandbox access).

Requires DAYTONA_API_KEY and a Windows snapshot (default windows-medium).

Tier 1/2 Daytona orgs currently cannot create Windows sandboxes; this script
exits with code 2 (skip) when Daytona returns that restriction so CI can
continue without failing the whole matrix.

Phase 1 checks (when Windows is available):
  1. Create ephemeral Windows sandbox
  2. Upload grok-*-windows-x86_64.exe
  3. Run --version / --help
"""

from __future__ import annotations

import os
import sys
import tempfile
import urllib.request
from pathlib import Path

from daytona import CreateSandboxFromSnapshotParams, Daytona, FileUpload
from daytona.common.errors import DaytonaForbiddenError


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
        dest = Path(tempfile.gettempdir()) / "grok-windows-smoke-artifact.exe"
        print(f"Downloading {url} -> {dest}")
        urllib.request.urlretrieve(url, dest)
        return dest
    fail("Set GROK_ARTIFACT_PATH or GROK_ARTIFACT_URL")


def main() -> None:
    if not os.environ.get("DAYTONA_API_KEY"):
        fail("DAYTONA_API_KEY is not set")

    artifact = resolve_artifact()
    ok(f"artifact {artifact} ({artifact.stat().st_size} bytes)")

    d = Daytona()
    snapshot = os.environ.get("DAYTONA_WINDOWS_SNAPSHOT", "windows-medium")
    try:
        sb = d.create(
            CreateSandboxFromSnapshotParams(
                snapshot=snapshot,
                labels={
                    "purpose": "grok-lhc-smoke",
                    "platform": "windows",
                    "project": "grok-build-lhc",
                },
                auto_stop_interval=15,
                ephemeral=True,
            ),
            timeout=float(os.environ.get("DAYTONA_CREATE_TIMEOUT", "240")),
        )
    except DaytonaForbiddenError as e:
        print(f"SKIP: Windows sandboxes not available for this org/key: {e}")
        raise SystemExit(2)
    except Exception as e:
        msg = str(e)
        if "Windows sandboxes are not available" in msg:
            print(f"SKIP: {msg}")
            raise SystemExit(2)
        raise

    ok(f"sandbox {sb.id} state={sb.state}")
    remote = r"C:\Users\daytona\grok.exe"
    # Paths vary by image; fall back to relative home if needed.
    try:
        sb.fs.upload_files(
            [FileUpload(source=str(artifact), destination=remote)]
        )
        ok(f"uploaded -> {remote}")
        r = sb.process.exec(f'cmd /c "{remote}" --version')
        print(r.result or "")
        if r.exit_code != 0:
            # try alternate path
            r2 = sb.process.exec(
                f'cmd /c "dir %USERPROFILE% & %USERPROFILE%\\grok.exe --version"'
            )
            print(r2.result or "")
            if r2.exit_code != 0:
                fail(f"--version failed: {r.exit_code} / {r2.exit_code}")
        ok("windows version ok")
        print("SMOKE_PASS windows")
    finally:
        try:
            d.delete(sb)
            ok("sandbox deleted")
        except Exception as e:
            print(f"WARN: delete sandbox: {e}", file=sys.stderr)


if __name__ == "__main__":
    main()
