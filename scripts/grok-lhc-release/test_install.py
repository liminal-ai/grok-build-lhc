#!/usr/bin/env python3
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts/grok-lhc-release/install.sh"

class InstallerTest(unittest.TestCase):
    def test_install_collision_update_uninstall_and_data_preservation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            candidate = root / "candidate"
            candidate.mkdir()
            asset = candidate / "grok-0.3.0-linux-x86_64"
            asset.write_text("#!/bin/sh\nprintf 'grok fixture\\n'\n", encoding="utf-8")
            asset.chmod(0o755)
            digest = hashlib.sha256(asset.read_bytes()).hexdigest()
            (candidate / "SHA256SUMS").write_text(f"{digest}  {asset.name}\n", encoding="utf-8")
            (candidate / "release-manifest.json").write_text("{}\n", encoding="utf-8")
            prefix, store, data = root / "prefix", root / "packages", root / "lhc-data"
            data.mkdir()
            (data / "keep").write_text("archive", encoding="utf-8")
            env = os.environ | {"HOME": str(root / "home")}
            base = ["sh", str(INSTALLER), "--version", "0.3.0", "--asset-dir", str(candidate), "--prefix", str(prefix), "--install-root", str(store)]
            result = subprocess.run(base, env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            command = prefix / "bin/grok"
            self.assertEqual(subprocess.check_output([command], text=True).strip(), "grok fixture")
            unmanaged = prefix / "bin/other"
            unmanaged.write_text("owned", encoding="utf-8")
            collision = subprocess.run(base + ["--name", "other"], env=env, text=True, capture_output=True)
            self.assertNotEqual(collision.returncode, 0)
            uninstall = subprocess.run(base + ["--uninstall"], env=env, text=True, capture_output=True)
            self.assertEqual(uninstall.returncode, 0, uninstall.stderr)
            self.assertFalse(command.exists())
            self.assertTrue((data / "keep").is_file())

    def test_custom_name_is_removed_without_repeating_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            candidate = root / "candidate"
            candidate.mkdir()
            asset = candidate / "grok-0.3.0-linux-x86_64"
            asset.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            digest = hashlib.sha256(asset.read_bytes()).hexdigest()
            (candidate / "SHA256SUMS").write_text(f"{digest}  {asset.name}\n", encoding="utf-8")
            (candidate / "release-manifest.json").write_text("{}\n", encoding="utf-8")
            prefix, store = root / "prefix", root / "packages"
            base = ["sh", str(INSTALLER), "--version", "0.3.0", "--asset-dir", str(candidate), "--prefix", str(prefix), "--install-root", str(store)]
            install = subprocess.run(base + ["--name", "grok-preview"], text=True, capture_output=True)
            self.assertEqual(install.returncode, 0, install.stderr)
            command = prefix / "bin/grok-preview"
            self.assertTrue(command.is_symlink())
            uninstall = subprocess.run(base + ["--uninstall"], text=True, capture_output=True)
            self.assertEqual(uninstall.returncode, 0, uninstall.stderr)
            self.assertFalse(command.exists())

    def test_refuses_unowned_install_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            candidate = root / "candidate"
            candidate.mkdir()
            asset = candidate / "grok-0.3.0-linux-x86_64"
            asset.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            digest = hashlib.sha256(asset.read_bytes()).hexdigest()
            (candidate / "SHA256SUMS").write_text(f"{digest}  {asset.name}\n", encoding="utf-8")
            (candidate / "release-manifest.json").write_text("{}\n", encoding="utf-8")
            store = root / "packages"
            store.mkdir()
            sentinel = store / "keep"
            sentinel.write_text("user-owned", encoding="utf-8")
            result = subprocess.run(
                ["sh", str(INSTALLER), "--version", "0.3.0", "--asset-dir", str(candidate), "--prefix", str(root / "prefix"), "--install-root", str(store)],
                env=os.environ | {"HOME": str(root / "home")},
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "user-owned")

    def test_refuses_command_name_change_for_managed_store(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            candidate = root / "candidate"
            candidate.mkdir()
            asset = candidate / "grok-0.3.0-linux-x86_64"
            asset.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            digest = hashlib.sha256(asset.read_bytes()).hexdigest()
            (candidate / "SHA256SUMS").write_text(f"{digest}  {asset.name}\n", encoding="utf-8")
            (candidate / "release-manifest.json").write_text("{}\n", encoding="utf-8")
            prefix, store = root / "prefix", root / "packages"
            base = ["sh", str(INSTALLER), "--version", "0.3.0", "--asset-dir", str(candidate), "--prefix", str(prefix), "--install-root", str(store)]
            first = subprocess.run(base + ["--name", "grok-preview"], text=True, capture_output=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            rename = subprocess.run(base + ["--name", "grok-other"], text=True, capture_output=True)
            self.assertNotEqual(rename.returncode, 0)
            self.assertTrue((prefix / "bin/grok-preview").is_symlink())
            self.assertFalse((prefix / "bin/grok-other").exists())

if __name__ == "__main__":
    unittest.main()
