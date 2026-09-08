#!/usr/bin/env python3
"""Installer lifecycle tests: local asset-dir mode, download mode against a
loopback server, receipts (name/prefix), platform selection, refusals."""
import hashlib
import http.server
import json
import os
import subprocess
import tempfile
import threading
import unittest
from functools import partial
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts/grok-lhc-release/install.sh"


def make_release(root: Path, version: str, platforms=("linux-x86_64",), body="#!/bin/sh\nexit 0\n") -> Path:
    """Lay down one release directory: assets, SHA256SUMS, release-manifest.json."""
    rel = root / f"release-{version}"
    rel.mkdir(parents=True, exist_ok=True)
    sums = []
    artifacts = []
    for platform in platforms:
        asset = rel / f"grok-{version}-{platform}"
        asset.write_text(body.replace("exit 0", f"printf '{platform} {version}\\n'"), encoding="utf-8")
        asset.chmod(0o755)
        digest = hashlib.sha256(asset.read_bytes()).hexdigest()
        sums.append(f"{digest}  {asset.name}\n")
        artifacts.append({"path": asset.name, "sha256": digest, "platform": platform})
    (rel / "SHA256SUMS").write_text("".join(sums), encoding="utf-8")
    (rel / "release-manifest.json").write_text(
        json.dumps({"product": "grok-lhc", "release_version": version, "artifacts": artifacts}, indent=2) + "\n",
        encoding="utf-8",
    )
    return rel


def run(args, env=None):
    return subprocess.run(["sh", str(INSTALLER), *args], env=env, text=True, capture_output=True)


class LocalReleaseServer:
    """Serves <base>/latest (GitHub-shaped JSON) and <base>/download/v<ver>/<asset>."""

    def __init__(self, releases: dict[str, Path], latest: str):
        self.root = Path(tempfile.mkdtemp())
        (self.root / "latest").write_text(json.dumps({"tag_name": f"v{latest}"}), encoding="utf-8")
        for version, rel in releases.items():
            dest = self.root / "download" / f"v{version}"
            dest.mkdir(parents=True)
            for item in rel.iterdir():
                (dest / item.name).write_bytes(item.read_bytes())
        class Quiet(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *args, **kwargs):
                pass

        handler = partial(Quiet, directory=str(self.root))
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.httpd.server_address[1]}"

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class InstallerTest(unittest.TestCase):
    def test_install_collision_update_uninstall_and_data_preservation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            candidate = make_release(root, "1.0.16")
            prefix, store, data = root / "prefix", root / "packages", root / "lhc-data"
            data.mkdir()
            (data / "keep").write_text("archive", encoding="utf-8")
            env = os.environ | {"HOME": str(root / "home")}
            base = ["--version", "1.0.16", "--asset-dir", str(candidate), "--prefix", str(prefix), "--install-root", str(store)]
            result = run(base, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            # Fresh default command name is grok-lhc; receipts record name, version, prefix.
            command = prefix / "bin/grok-lhc"
            self.assertEqual(subprocess.check_output([command], text=True).strip(), "linux-x86_64 1.0.16")
            self.assertEqual((store / "installed-name").read_text().strip(), "grok-lhc")
            self.assertEqual((store / "installed-version").read_text().strip(), "1.0.16")
            self.assertEqual((store / "installed-prefix").read_text().strip(), str(prefix))
            self.assertEqual((store / "current").resolve(), (store / "versions/1.0.16").resolve())
            unmanaged = prefix / "bin/other"
            unmanaged.write_text("owned", encoding="utf-8")
            collision = run(base + ["--name", "other"], env)
            self.assertNotEqual(collision.returncode, 0)
            uninstall = run(base + ["--uninstall"], env)
            self.assertEqual(uninstall.returncode, 0, uninstall.stderr)
            self.assertFalse(command.exists())
            self.assertTrue((data / "keep").is_file())

    def test_update_reuses_recorded_name_and_prefix_and_keeps_old_versions(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = make_release(root, "1.0.16")
            second = make_release(root, "1.0.16-lhc.1")
            prefix, store = root / "custom-prefix", root / "custom-store"
            env = os.environ | {"HOME": str(root / "home")}
            install = run(["--version", "1.0.16", "--asset-dir", str(first), "--name", "grok-memory",
                           "--prefix", str(prefix), "--install-root", str(store)], env)
            self.assertEqual(install.returncode, 0, install.stderr)
            # Update names only the store: name and prefix come from receipts.
            update = run(["--version", "1.0.16-lhc.1", "--asset-dir", str(second), "--install-root", str(store)], env)
            self.assertEqual(update.returncode, 0, update.stderr)
            command = prefix / "bin/grok-memory"
            self.assertEqual(subprocess.check_output([command], text=True).strip(), "linux-x86_64 1.0.16-lhc.1")
            self.assertEqual((store / "installed-version").read_text().strip(), "1.0.16-lhc.1")
            self.assertEqual((store / "installed-name").read_text().strip(), "grok-memory")
            self.assertEqual((store / "installed-prefix").read_text().strip(), str(prefix))
            self.assertTrue((store / "versions/1.0.16/bin/grok").is_file(), "old versions are kept")
            self.assertFalse((root / "home/.local/bin/grok-lhc").exists(), "default prefix must not be used")
            # Uninstall also takes the prefix from the receipt.
            uninstall = run(["--uninstall", "--install-root", str(store)], env)
            self.assertEqual(uninstall.returncode, 0, uninstall.stderr)
            self.assertFalse(command.exists())
            self.assertFalse(store.exists())

    def test_transition_store_without_prefix_receipt_takes_explicit_prefix(self):
        """The 0.3.1 layout: installed-name=grok, no installed-prefix. One rerun with --prefix
        against the existing store keeps the name and link and records the prefix."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            old = make_release(root, "0.3.1")
            new = make_release(root, "1.0.16")
            prefix, store = root / "prefix", root / "packages"
            env = os.environ | {"HOME": str(root / "home")}
            install = run(["--version", "0.3.1", "--asset-dir", str(old), "--name", "grok",
                           "--prefix", str(prefix), "--install-root", str(store)], env)
            self.assertEqual(install.returncode, 0, install.stderr)
            (store / "installed-prefix").unlink()  # what a 0.3.1 store looks like
            # A second, unmanaged-by-us symlink next to it (grok-lhc -> same file) must survive untouched.
            extra = prefix / "bin/grok-lhc"
            extra.symlink_to(store / "current/bin/grok")
            rerun = run(["--version", "1.0.16", "--asset-dir", str(new), "--prefix", str(prefix), "--install-root", str(store)], env)
            self.assertEqual(rerun.returncode, 0, rerun.stderr)
            self.assertEqual((store / "installed-name").read_text().strip(), "grok")
            self.assertEqual((store / "installed-prefix").read_text().strip(), str(prefix))
            self.assertEqual(subprocess.check_output([prefix / "bin/grok"], text=True).strip(), "linux-x86_64 1.0.16")
            self.assertEqual(subprocess.check_output([extra], text=True).strip(), "linux-x86_64 1.0.16")
            self.assertTrue((store / "versions/0.3.1").is_dir())

    def test_download_mode_resolves_latest_and_selects_platform(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            rel = make_release(root, "1.0.16-lhc.2", platforms=("linux-x86_64", "darwin-aarch64"))
            server = LocalReleaseServer({"1.0.16-lhc.2": rel}, latest="1.0.16-lhc.2")
            try:
                prefix, store = root / "prefix", root / "packages"
                env = os.environ | {"HOME": str(root / "home"), "GROK_LHC_RELEASE_BASE": server.base}
                # No --version: latest resolved from <base>/latest; host platform selected by uname.
                result = run(["--download", "--prefix", str(prefix), "--install-root", str(store)], env)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((store / "installed-version").read_text().strip(), "1.0.16-lhc.2")
                host = subprocess.check_output([prefix / "bin/grok-lhc"], text=True).strip()
                self.assertTrue(host.startswith(("linux-", "darwin-")), host)
                # Mac ARM64 selection against the same local assets (no darwin build here).
                mac_store = root / "mac-packages"
                result = run(["--download", "--version", "1.0.16-lhc.2", "--platform", "darwin-aarch64",
                              "--name", "grok-mac", "--prefix", str(prefix), "--install-root", str(mac_store)], env)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((mac_store / "versions/1.0.16-lhc.2/bin/grok").read_text().count("darwin-aarch64"), 1)
                # Unknown platform and a missing release both refuse.
                self.assertNotEqual(run(["--download", "--version", "1.0.16-lhc.2", "--platform", "windows-x86_64",
                                         "--install-root", str(root / "w")], env).returncode, 0)
                self.assertNotEqual(run(["--download", "--version", "9.9.9", "--install-root", str(root / "m")], env).returncode, 0)
                self.assertFalse((root / "m").exists())
            finally:
                server.close()

    def test_refuses_checksum_mismatch_and_wrong_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            rel = make_release(root, "1.0.16")
            store = root / "packages"
            env = os.environ | {"HOME": str(root / "home")}
            asset = rel / "grok-1.0.16-linux-x86_64"
            asset.write_text("#!/bin/sh\nprintf tampered\n", encoding="utf-8")
            result = run(["--version", "1.0.16", "--asset-dir", str(rel), "--prefix", str(root / "p"), "--install-root", str(store)], env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("checksum mismatch", result.stderr)
            self.assertFalse(store.exists())
            rel2 = make_release(root, "1.0.16-lhc.1")
            (rel2 / "release-manifest.json").write_text('{"release_version": "0.0.0"}\n', encoding="utf-8")
            result = run(["--version", "1.0.16-lhc.1", "--asset-dir", str(rel2), "--prefix", str(root / "p"), "--install-root", str(store)], env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not for 1.0.16-lhc.1", result.stderr)

    def test_refuses_unowned_install_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            candidate = make_release(root, "1.0.16")
            store = root / "packages"
            store.mkdir()
            sentinel = store / "keep"
            sentinel.write_text("user-owned", encoding="utf-8")
            result = run(["--version", "1.0.16", "--asset-dir", str(candidate), "--prefix", str(root / "prefix"), "--install-root", str(store)],
                         os.environ | {"HOME": str(root / "home")})
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "user-owned")

    def test_refuses_command_name_change_for_managed_store(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            candidate = make_release(root, "1.0.16")
            prefix, store = root / "prefix", root / "packages"
            base = ["--version", "1.0.16", "--asset-dir", str(candidate), "--prefix", str(prefix), "--install-root", str(store)]
            first = run(base + ["--name", "grok-preview"])
            self.assertEqual(first.returncode, 0, first.stderr)
            rename = run(base + ["--name", "grok-other"])
            self.assertNotEqual(rename.returncode, 0)
            self.assertTrue((prefix / "bin/grok-preview").is_symlink())
            self.assertFalse((prefix / "bin/grok-other").exists())
            # Invalid release strings never reach the store.
            bad = run(["--version", "1.0.16-alpha.1", "--asset-dir", str(candidate), "--prefix", str(prefix), "--install-root", str(root / "bad")])
            self.assertNotEqual(bad.returncode, 0)


if __name__ == "__main__":
    unittest.main()
