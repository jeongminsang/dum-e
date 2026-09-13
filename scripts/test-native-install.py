#!/usr/bin/env python3
"""Exercise the installer against a real native release archive and local HTTP."""
import argparse
import functools
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args):
        pass


class NativeInstallTests(unittest.TestCase):
    archive: Path
    version: str

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="dume-install-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.payload = self.root / "releases" / ("v" + self.version)
        self.payload.mkdir(parents=True)
        shutil.copyfile(self.archive, self.payload / self.archive.name)
        with self.archive.open("rb") as content:
            digest = hashlib.file_digest(content, "sha256").hexdigest()
        (self.payload / "SHA256SUMS").write_text(f"{digest}  {self.archive.name}\n")
        api = self.root / "repos" / "jeongminsang" / "dum-e" / "releases"
        api.mkdir(parents=True)
        self.latest = api / "latest"
        self.latest.write_text(json.dumps({"tag_name": "v" + self.version}))
        handler = functools.partial(QuietHandler, directory=str(self.root))
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop_server)
        self.install_dir = self.root / "installed"
        self.install_dir.mkdir()
        self.previous = self.install_dir / "dume"
        self.previous.write_bytes(b"previous installation")
        self.commands = self.root / "commands"
        self.commands.mkdir()
        # An explicit executable allowlist excludes Node, Bun, Cargo and Python.
        for name in ("uname", "curl", "tar", "sed", "grep", "awk", "wc", "tr",
                     "mktemp", "sha256sum", "shasum", "chmod", "cp", "mv", "rm", "mkdir"):
            executable = shutil.which(name)
            if executable:
                (self.commands / name).symlink_to(executable)
        self.env = dict(os.environ, PATH=str(self.commands),
                        DUME_INSTALL_DIR=str(self.install_dir),
                        DUME_GITHUB_API=f"http://127.0.0.1:{self.server.server_port}",
                        DUME_GITHUB_RELEASES=f"http://127.0.0.1:{self.server.server_port}/releases")
        self.env.pop("GITHUB_TOKEN", None)
        self.installer = Path(__file__).resolve().parent / "install.sh"

    def stop_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def install(self, *args):
        return subprocess.run(["/bin/sh", str(self.installer), *args], env=self.env,
                              cwd=self.root, text=True, capture_output=True, timeout=60)

    def assert_preserved(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(self.previous.read_bytes(), b"previous installation")
        self.assertEqual(sorted(p.name for p in self.install_dir.iterdir()), ["dume"])

    def test_latest_installs_real_binary_without_node_bun_or_cargo(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        version = subprocess.check_output([str(self.previous), "--version"], env=self.env,
                                          cwd=self.root, text=True).strip()
        self.assertEqual(version, f"dume {self.version}")
        self.assertFalse((self.commands / "node").exists())
        self.assertFalse((self.commands / "bun").exists())
        self.assertFalse((self.commands / "cargo").exists())

    def test_checksum_mismatch_preserves_existing_executable(self):
        (self.payload / "SHA256SUMS").write_text(f"{'0' * 64}  {self.archive.name}\n")
        result = self.install("--ref", "v" + self.version)
        self.assert_preserved(result)
        self.assertIn("checksum mismatch", result.stderr)

    def test_missing_and_duplicate_checksums_are_rejected(self):
        checksum = self.payload / "SHA256SUMS"
        valid = checksum.read_text()
        for invalid in ("", valid + valid):
            with self.subTest(checksum=invalid):
                checksum.write_text(invalid)
                self.assert_preserved(self.install("--ref", "v" + self.version))

    def test_invalid_latest_has_no_fallback_release(self):
        self.latest.write_text(json.dumps({"tag_name": "../v0.1.0"}))
        result = self.install()
        self.assert_preserved(result)
        self.assertIn("no fallback", result.stderr)

    def test_valid_prerelease_tag_with_wrong_binary_version_is_rejected(self):
        tag = "v" + self.version + "-rc.1+build.2"
        shutil.copytree(self.payload, self.payload.parent / tag)
        result = self.install("--ref", tag)
        self.assert_preserved(result)
        self.assertIn("binary version mismatch", result.stderr)

    def test_dev_and_release_are_mutually_exclusive(self):
        self.assert_preserved(self.install("--dev", "--ref", "v" + self.version))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    NativeInstallTests.archive = args.archive.resolve(strict=True)
    NativeInstallTests.version = args.version
    unittest.main(argv=[__file__], verbosity=2)
