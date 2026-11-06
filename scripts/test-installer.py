#!/usr/bin/env python3
"""Test the POSIX installer with packaged binaries and local release fixtures."""

import hashlib
import http.server
import io
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest


DIST = Path(sys.argv.pop(1) if len(sys.argv) > 1 else "dist").resolve()
ROOT = Path(__file__).resolve().parent.parent


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args):
        pass


class InstallerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(
            platform.machine(), platform.machine()
        )
        system = "apple-darwin" if sys.platform == "darwin" else "unknown-linux-musl"
        if sys.platform == "darwin" and arch == "x86_64":
            native = subprocess.run(
                ["sysctl", "-n", "hw.optional.arm64"], capture_output=True, text=True
            )
            if native.stdout.strip() == "1":
                arch = "aarch64"
        cls.name = f"vayu-{arch}-{system}"
        cls.asset = f"{cls.name}.tar.gz"
        cls.archive = DIST / cls.asset
        if not cls.archive.is_file():
            raise RuntimeError(f"{cls.archive} not found; run make package for this host")
        sums = DIST / "SHA256SUMS"
        if not sums.is_file():
            sums = DIST / f"{cls.asset}.sha256"
        cls.sums = sums.read_text()
        with tarfile.open(cls.archive) as archive:
            for doc in ("README.md", "CHANGELOG.md", "LICENSE"):
                archive.getmember(f"{cls.name}/{doc}")
            binary = archive.getmember(f"{cls.name}/vayu")
            if not binary.mode & 0o111:
                raise RuntimeError("packaged vayu is not executable")
            cls.binary_hash = hashlib.sha256(archive.extractfile(binary).read()).hexdigest()
        cls.temp = tempfile.TemporaryDirectory(prefix="vayu-installer-")
        cls.server_root = Path(cls.temp.name)
        # Some VPNs allow binding IPv4 loopback but reject connections to it.
        for family, host in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
            class LoopbackServer(http.server.ThreadingHTTPServer):
                address_family = family

            server = LoopbackServer(
                (host, 0),
                lambda *args, **kwargs: QuietHandler(
                    *args, directory=str(cls.server_root), **kwargs
                ),
            )
            try:
                with socket.create_connection((host, server.server_port), timeout=2):
                    pass
            except OSError:
                server.server_close()
                if family == socket.AF_INET6:
                    raise
                continue
            cls.server = server
            cls.host = f"[{host}]" if family == socket.AF_INET6 else host
            break
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.installer = DIST / "install.sh"
        if not cls.installer.is_file():
            cls.installer = ROOT / "scripts/install.sh"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()
        cls.temp.cleanup()

    def setUp(self):
        self.work = Path(tempfile.mkdtemp(dir=self.server_root))
        self.release = self.work / "release"
        self.release.mkdir()
        self.write_release(self.release)
        self.base = (
            f"http://{self.host}:{self.server.server_port}/{self.work.name}/release"
        )
        self.home = self.work / "home"
        self.home.mkdir()
        self.tools = self.work / "tools"
        self.tools.mkdir()
        # Isolate PATH from any vayu already installed on the developer's machine.
        for name in (
            "uname", "sysctl", "cat", "curl", "wget", "sha256sum", "shasum", "perl",
            "cut", "awk", "grep", "mktemp", "rm", "gzip", "tar", "mkdir", "cp",
            "chmod", "mv", "wc",
        ):
            executable = shutil.which(name)
            if executable:
                (self.tools / name).symlink_to(executable)
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("VAYU_") and key.lower() not in (
                "http_proxy", "https_proxy", "all_proxy"
            )
        }
        self.env.update(
            HOME=str(self.home), PATH=str(self.tools), VAYU_BASE_URL=self.base,
            NO_PROXY="*", no_proxy="*", TMPDIR=str(self.work / "tmp"),
        )
        Path(self.env["TMPDIR"]).mkdir()
        self.dest = self.home / ".local/bin/vayu"

    def tearDown(self):
        shutil.rmtree(self.work)

    def write_release(self, directory):
        directory.mkdir(parents=True, exist_ok=True)
        (directory / self.asset).symlink_to(self.archive)
        (directory / "SHA256SUMS").write_text(self.sums)

    def run_installer(self, *args, ok=True, piped=False):
        command = ["/bin/sh", str(self.installer), *args]
        source = None
        if piped:
            command = ["/bin/sh", "-s", "--", *args]
            source = self.installer.read_text()
        result = subprocess.run(
            command, input=source, env=self.env, text=True, capture_output=True, timeout=30
        )
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(list(Path(self.env["TMPDIR"]).iterdir()), [])
        return result

    def assert_installed(self, dest=None):
        dest = dest or self.dest
        self.assertEqual(hashlib.sha256(dest.read_bytes()).hexdigest(), self.binary_hash)
        self.assertTrue(os.access(dest, os.X_OK))
        result = subprocess.run([str(dest), "--version"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("(vayu)", result.stdout)
        self.assertEqual(list(dest.parent.glob(".vayu.install.*")), [])

    def executable(self, path, contents):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\n" + contents + "\n")
        path.chmod(0o755)

    def test_default_install_from_a_pipe(self):
        result = self.run_installer(piped=True)
        self.assert_installed()
        # Not a terminal: plain text, and the next step for a directory missing from PATH.
        self.assertNotIn("\033", result.stderr)
        self.assertIn("Installed vayu", result.stderr)
        self.assertIn("is not on your PATH", result.stderr)

    def test_verbose_shows_diagnostic_details(self):
        result = self.run_installer("--verbose")
        self.assert_installed()
        self.assertIn(f"{self.base}/{self.asset}", result.stderr)
        self.assertIn(hashlib.sha256(self.archive.read_bytes()).hexdigest(), result.stderr)

    def test_help_from_a_pipe(self):
        result = self.run_installer("--help", piped=True)
        self.assertIn("Usage:", result.stdout)
        self.assertIn("--version", result.stdout)
        self.assertFalse(self.dest.exists())

    def test_environment_directory_and_explicit_override(self):
        self.env["VAYU_INSTALL_DIR"] = str(self.work / "environment bin")
        self.run_installer()
        self.assert_installed(Path(self.env["VAYU_INSTALL_DIR"]) / "vayu")
        explicit = self.work / "explicit bin"
        self.run_installer("--dir", str(explicit))
        self.assert_installed(explicit / "vayu")

    def test_upgrade_and_reinstall(self):
        self.executable(self.dest, "echo 'vayu 0.9.0 (vayu)'")
        result = self.run_installer()
        self.assert_installed()
        self.assertIn("Updated vayu from 0.9.0", result.stderr)
        result = self.run_installer()
        self.assert_installed()
        self.assertIn("Reinstalled vayu", result.stderr)

    def test_unrelated_destination_is_preserved_unless_forced(self):
        self.executable(self.dest, "echo 'another program'")
        before = self.dest.read_bytes()
        result = self.run_installer(ok=False)
        self.assertIn("different program", result.stderr)
        self.assertEqual(self.dest.read_bytes(), before)
        self.run_installer("--force")
        self.assert_installed()

    def test_unrelated_program_on_path(self):
        other = self.tools / "vayu"
        self.executable(other, "echo 'another program'")
        before = other.read_bytes()
        self.run_installer(ok=False)
        self.assertFalse(self.dest.exists())
        self.run_installer("--force")
        self.assert_installed()
        self.assertEqual(other.read_bytes(), before)

    def test_program_on_path_cannot_consume_a_piped_installer(self):
        # bash, /bin/sh on macOS and some Linux distributions, reads a piped script byte by byte.
        self.executable(self.tools / "vayu", "cat >/dev/null\necho 'vayu 0.9.0 (vayu)'")
        self.run_installer(piped=True)
        self.assert_installed()

    def test_directory_destination_is_refused_even_when_forced(self):
        self.dest.mkdir(parents=True)
        result = self.run_installer("--force", ok=False)
        self.assertIn("is a directory", result.stderr)
        self.assertEqual(list(self.dest.iterdir()), [])
        self.assertEqual(list(self.dest.parent.glob(".vayu.install.*")), [])

    def test_checksum_mismatch_preserves_an_existing_installation(self):
        archive = self.release / self.asset
        archive.unlink()
        archive.write_bytes(b"corrupt archive")
        self.executable(self.dest, "echo 'vayu 0.9.0 (vayu)'")
        before = self.dest.read_bytes()
        result = self.run_installer(ok=False)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertEqual(self.dest.read_bytes(), before)

    def test_missing_checksum_entry(self):
        (self.release / "SHA256SUMS").write_text("0" * 64 + "  another.tar.gz\n")
        result = self.run_installer(ok=False)
        self.assertIn("no entry", result.stderr)
        self.assertFalse(self.dest.exists())

    def test_missing_archive(self):
        (self.release / self.asset).unlink()
        result = self.run_installer(ok=False)
        self.assertIn("could not download", result.stderr)
        self.assertIn(f"{self.base}/{self.asset}", result.stderr)
        self.assertIn("/issues", result.stderr)
        self.assertFalse(self.dest.exists())

    def test_invalid_executable(self):
        archive = self.release / self.asset
        archive.unlink()
        with tarfile.open(archive, "w:gz") as output:
            contents = b"#!/bin/sh\nexit 1\n"
            member = tarfile.TarInfo(f"{self.name}/vayu")
            member.size = len(contents)
            member.mode = 0o755
            output.addfile(member, io.BytesIO(contents))
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.release / "SHA256SUMS").write_text(f"{digest}  {self.asset}\n")
        result = self.run_installer(ok=False)
        self.assertIn("does not run", result.stderr)
        self.assertFalse(self.dest.exists())
        self.assertEqual(list(self.dest.parent.glob(".vayu.install.*")), [])

    def test_invalid_arguments(self):
        for args in (("--unknown",), ("--dir",), ("--version",), ("--dir", ""),
                     ("--version", "")):
            with self.subTest(args=args):
                self.run_installer(*args, ok=False)
                self.assertFalse(self.dest.exists())

    def test_unsupported_platform(self):
        uname = self.tools / "uname"
        uname.unlink()
        self.executable(uname, "echo Unsupported")
        result = self.run_installer(ok=False)
        self.assertIn("no prebuilt vayu", result.stderr)
        self.assertFalse(self.dest.exists())

    def test_github_latest_and_pinned_release_urls(self):
        # Redirect the real downloader to the fixture server, retaining GitHub's URL paths.
        fetcher = "curl" if (self.tools / "curl").exists() else "wget"
        shim = self.tools / fetcher
        # Retain the applet name when wget is a symlink to the BusyBox executable.
        real = os.readlink(shim)
        shim.unlink()
        shim.write_text(
            f"#!{sys.executable}\n"
            "import os, sys\n"
            "args = sys.argv[1:]\n"
            "prefix = 'https://github.com/fixture/vayu/releases/'\n"
            "assert args[-1].startswith(prefix), args[-1]\n"
            f"args[-1] = {self.base!r} + '/' + args[-1][len(prefix):]\n"
            # GitHub downloads are HTTPS-only; allow the fixture server's plain HTTP.
            "if '--proto' in args:\n"
            "    i = args.index('--proto') + 1\n"
            "    assert args[i] == '=https', args[i]\n"
            "    args[i] = '=http,https'\n"
            f"os.execv({real!r}, [{real!r}, *args])\n"
        )
        shim.chmod(0o755)
        self.env.pop("VAYU_BASE_URL")
        self.env["VAYU_REPO"] = "fixture/vayu"
        self.write_release(self.release / "latest/download")
        self.write_release(self.release / "download/v1.0.0")
        for args in ((), ("--version", "1.0.0"), ("--version", "v1.0.0")):
            with self.subTest(args=args):
                self.run_installer(*args)
                self.assert_installed()


if __name__ == "__main__":
    unittest.main(verbosity=2)
