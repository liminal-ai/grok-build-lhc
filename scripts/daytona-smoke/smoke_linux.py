#!/usr/bin/env python3
"""Daytona Linux smoke for grok-build-lhc release artifacts.

Requires:
  DAYTONA_API_KEY

Inputs (env):
  GROK_CANDIDATE_DIR — immutable candidate directory
  GROK_LHC_VERSION — candidate version

Checks:
  1. Create ephemeral Linux sandbox (daytona-medium)
  2. Upload and install the exact checksummed candidate
  3. Dynamic linker / shared libs resolve (ldd)
  4. `grok --version` exits 0 and prints a version
  5. `grok --help` exits 0
  6. Run a deterministic headless turn and verify prompt, assistant response,
     the manifest's LHC thread schema, and completed turn durability after
     process exit
  7. Uninstall managed files while preserving user/LHC data
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path

from daytona import CreateSandboxFromSnapshotParams, Daytona, FileUpload


def fail(msg: str, code: int = 1) -> None:
    print(f"FAIL: {msg}", file=sys.stderr)
    raise SystemExit(code)


def ok(msg: str) -> None:
    print(f"OK: {msg}")


def candidate_dir() -> Path:
    candidate = Path(os.environ.get("GROK_CANDIDATE_DIR", ""))
    if not candidate.is_dir():
        fail(f"GROK_CANDIDATE_DIR not a directory: {candidate}")
    return candidate


def require_key() -> None:
    if not os.environ.get("DAYTONA_API_KEY"):
        fail("DAYTONA_API_KEY is not set")


def main() -> None:
    require_key()
    candidate = candidate_dir()
    version = os.environ.get("GROK_LHC_VERSION", "")
    if not version:
        fail("GROK_LHC_VERSION is not set")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[.-][0-9A-Za-z.-]+)?", version):
        fail(f"invalid GROK_LHC_VERSION: {version}")
    artifact = candidate / f"grok-{version}-linux-x86_64"
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

    remote_candidate = "/home/daytona/candidate"
    remote_installer = f"{remote_candidate}/install.sh"
    remote_lifecycle = "/home/daytona/grok_lhc_lifecycle.py"
    remote = "/home/daytona/prefix/bin/grok-lhc-smoke"
    try:
        mkdir = sb.process.exec(f"mkdir -p {remote_candidate}")
        if mkdir.exit_code != 0:
            fail(f"could not create candidate directory: {mkdir.result}")
        uploads = [
            FileUpload(source=str(path), destination=f"{remote_candidate}/{path.name}")
            for path in candidate.iterdir() if path.is_file()
        ]
        lifecycle = Path(__file__).with_name("grok_lhc_lifecycle.py")
        uploads.append(FileUpload(source=str(lifecycle), destination=remote_lifecycle))
        sb.fs.upload_files(uploads)
        ok(f"uploaded immutable candidate -> {remote_candidate}")

        install = sb.process.exec(
            f"HOME=/home/daytona/home sh {remote_installer} --version {version} --name grok-lhc-smoke --prefix /home/daytona/prefix --install-root /home/daytona/packages --asset-dir {remote_candidate}",
            timeout=60,
        )
        if install.exit_code != 0:
            fail(f"install failed: {install.result}")

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

        persistence = sb.process.exec(
            f"python3 {remote_lifecycle} {remote} /home/daytona/home /home/daytona/lhc-data {remote_candidate}/release-manifest.json",
            timeout=90,
        )
        print(persistence.result or "")
        if persistence.exit_code != 0 or "LHC_PERSISTENCE_PASS" not in (persistence.result or ""):
            fail(f"default-on LHC persistence failed: {persistence.exit_code}")
        ok("default-on LHC prompt/assistant/turn persistence")

        uninstall = sb.process.exec(
            f"HOME=/home/daytona/home sh {remote_installer} --name grok-lhc-smoke --prefix /home/daytona/prefix --install-root /home/daytona/packages --uninstall && test ! -e {remote}",
            timeout=60,
        )
        if uninstall.exit_code != 0:
            fail(f"uninstall/data preservation failed: {uninstall.result}")
        preserved = sb.process.exec(
            f"python3 {remote_lifecycle} --verify-only /home/daytona/lhc-data {remote_candidate}/release-manifest.json",
            timeout=60,
        )
        if preserved.exit_code != 0 or "LHC_PERSISTENCE_PASS" not in (preserved.result or ""):
            fail(f"uninstall did not preserve captured LHC data: {preserved.result}")
        ok("uninstall preserved captured LHC data")

        print("SMOKE_PASS linux")
    finally:
        try:
            d.delete(sb)
            ok("sandbox deleted")
        except Exception as e:
            print(f"WARN: delete sandbox: {e}", file=sys.stderr)


if __name__ == "__main__":
    main()
