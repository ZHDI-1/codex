import hashlib
import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("bootstrap.sh")
TAG_URL = "https://github.com/ZHDI-1/codex/releases/tag/rust-v0.166.0%2Bpatched.2"
INSTALLER = r'''#!/bin/sh
set -eu
printf 'install:%s:%s:%s\n' "$CODEX_INSTALL_DAEMON_ONLY" "$CODEX_RELEASE" "$CODEX_NON_INTERACTIVE" >> "$CODEX_BOOTSTRAP_TEST_STATE/install.log"
if [ "${CODEX_BOOTSTRAP_TEST_FAIL_INSTALL:-}" = "$CODEX_INSTALL_DAEMON_ONLY" ]; then exit 42; fi
mkdir -p "$CODEX_INSTALL_DIR"
cat > "$CODEX_INSTALL_DIR/codex" <<'CODEX'
#!/bin/sh
printf '%s\n' "$*" >> "$CODEX_BOOTSTRAP_TEST_STATE/commands.log"
if [ "$*" = 'app-server daemon restart' ] && [ "${CODEX_BOOTSTRAP_TEST_FAIL_RESTART:-}" = 1 ]; then exit 7; fi
if [ "$*" = --version ]; then printf 'codex-cli 0.166.0+patched.2\n'; fi
CODEX
chmod +x "$CODEX_INSTALL_DIR/codex"
'''

CURL = '#!' + sys.executable + r'''
import hashlib, json, os, pathlib, sys
root = pathlib.Path(os.environ['CODEX_BOOTSTRAP_TEST_STATE'])
args = sys.argv[1:]
url = next(a for a in args if a.startswith('https://'))
with (root/'requests.log').open('a') as stream:
    stream.write(url+'\n')
if '-w' in args:
    if os.environ.get('CODEX_BOOTSTRAP_TEST_FAIL_METADATA') == '1': sys.exit(22)
    print(os.environ['CODEX_BOOTSTRAP_TEST_TAG'], end='')
else:
    if os.environ.get('CODEX_BOOTSTRAP_TEST_FAIL_DOWNLOAD') == '1': sys.exit(22)
    destination = pathlib.Path(args[args.index('-o')+1])
    installer = (root/'fixture-installer.sh').read_bytes()
    if url.endswith('/SHA256SUMS'):
        digest = '0'*64 if os.environ.get('CODEX_BOOTSTRAP_TEST_BAD_SHA') == '1' else hashlib.sha256(installer).hexdigest()
        manifest = digest+'  install.sh\n'
        for target in ('x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl',
                       'x86_64-apple-darwin', 'aarch64-apple-darwin'):
            if target != os.environ.get('CODEX_BOOTSTRAP_TEST_MISSING_TARGET'):
                manifest += '1'*64+'  codex-package-'+target+'.tar.gz\n'
        destination.write_text(manifest)
    else:
        destination.write_bytes(installer)
    with (root/'outputs.log').open('a') as stream:
        stream.write(str(destination)+'\n')
'''


class BootstrapTest(unittest.TestCase):
    def run_case(self, *options, existing=False, architecture="x86_64", operating_system="Linux",
                 hash_tool="sha256sum", translated="0", **extra_env):
        with tempfile.TemporaryDirectory(prefix="codex-bootstrap-test-") as directory:
            root = Path(directory)
            tools = root / "tools"
            tools.mkdir()
            # Restrict PATH to exercise stock-macOS hash fallbacks on any host.
            for name in ("bash", "sh", "awk", "sed", "mktemp", "rm", "mkdir", "cat", "chmod", hash_tool):
                executable = shutil.which(name)
                if executable is None:
                    self.skipTest(f"hash/tool fixture unavailable: {name}")
                (tools / name).symlink_to(executable)
            (tools / "curl").write_text(CURL)
            (tools / "curl").chmod(0o755)
            # A different codex already on PATH must never be used for activation.
            (tools / "codex").write_text("#!/bin/sh\nexit 55\n")
            (tools / "codex").chmod(0o755)
            (tools / "uname").write_text(
                '#!/bin/sh\nif [ "$1" = -s ]; then echo '+operating_system+'; else echo '+architecture+'; fi\n'
            )
            (tools / "uname").chmod(0o755)
            (tools / "sysctl").write_text('#!/bin/sh\necho '+translated+'\n')
            (tools / "sysctl").chmod(0o755)
            install_dir = root / "installed commands"
            if existing:
                install_dir.mkdir()
                (install_dir / "codex").write_text("#!/bin/sh\nexit 56\n")
                (install_dir / "codex").chmod(0o755)
            (root / "fixture-installer.sh").write_text(INSTALLER)
            environment = os.environ.copy()
            environment.update({
                "PATH": str(tools),
                "CODEX_INSTALL_DIR": str(install_dir),
                "CODEX_BOOTSTRAP_TEST_STATE": str(root),
                "CODEX_BOOTSTRAP_TEST_TAG": TAG_URL,
                # Inherited pins and daemon-only selections must not leak in.
                "CODEX_RELEASE": "0.164.0",
                "CODEX_INSTALL_DAEMON_ONLY": "1",
                "CODEX_NON_INTERACTIVE": "0",
                **extra_env,
            })
            result = subprocess.run(["bash", "-s", "--", *options], input=SCRIPT.read_text(),
                                    text=True, capture_output=True, env=environment, timeout=10)
            def lines(name):
                path = root / name
                return path.read_text().splitlines() if path.exists() else []
            installs, commands, requests, outputs = (lines(n) for n in (
                "install.log", "commands.log", "requests.log", "outputs.log"))
            for output in outputs:
                self.assertFalse(Path(output).parent.exists(), "temporary downloads must be cleaned")
            return result, installs, commands, requests

    def test_fresh_and_existing_installs_through_piped_bash(self):
        for existing in (False, True):
            with self.subTest(existing=existing):
                result, installs, commands, requests = self.run_case(existing=existing)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(installs, ["install:0:latest:1", "install:1:latest:1"])
                self.assertEqual(commands, ["app-server daemon restart", "--version", "app-server daemon version"])
                self.assertEqual(requests[1:], [TAG_URL.replace('/tag/', '/download/')+'/'+name
                                               for name in ('install.sh', 'SHA256SUMS')])

    def test_no_restart_stages_both_packages(self):
        result, installs, commands, _ = self.run_case("--no-restart")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(installs), 2)
        self.assertEqual(commands, ["--version"])
        self.assertIn("restart deferred", result.stdout)

    def test_bad_checksum_stops_before_installation(self):
        result, installs, commands, _ = self.run_case(CODEX_BOOTSTRAP_TEST_BAD_SHA="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((installs, commands), ([], []))
        self.assertIn("checksum mismatch", result.stderr)

    def test_failed_download_stops_before_installation(self):
        for variable in ("CODEX_BOOTSTRAP_TEST_FAIL_METADATA", "CODEX_BOOTSTRAP_TEST_FAIL_DOWNLOAD"):
            with self.subTest(variable=variable):
                result, installs, commands, _ = self.run_case(**{variable: "1"})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual((installs, commands), ([], []))

    def test_failed_install_never_restarts(self):
        for stage in ("0", "1"):
            with self.subTest(stage=stage):
                result, installs, commands, _ = self.run_case(CODEX_BOOTSTRAP_TEST_FAIL_INSTALL=stage)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(installs), int(stage)+1)
                self.assertEqual(commands, [])

    def test_restart_failure_is_propagated(self):
        result, _, commands, _ = self.run_case(CODEX_BOOTSTRAP_TEST_FAIL_RESTART="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(commands, ["app-server daemon restart"])

    def test_unsupported_machine_fails_without_network(self):
        result, installs, commands, requests = self.run_case(architecture="riscv64")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((installs, commands, requests), ([], [], []))

    def test_all_four_platforms_and_upstream_architecture_aliases(self):
        for system, arch, target in (
            ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
            ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
            ("Darwin", "arm64", "aarch64-apple-darwin"),
            ("Linux", "amd64", "x86_64-unknown-linux-musl"),
            ("Linux", "arm64", "aarch64-unknown-linux-musl"),
            ("Darwin", "aarch64", "aarch64-apple-darwin"),
        ):
            with self.subTest(system=system, arch=arch):
                result, installs, _, _ = self.run_case(architecture=arch, operating_system=system)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(installs), 2)
                self.assertIn("for "+target, result.stdout)

    def test_rosetta_prefers_native_apple_silicon(self):
        result, installs, _, _ = self.run_case(operating_system="Darwin", translated="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(installs), 2)
        self.assertIn("for aarch64-apple-darwin", result.stdout)

    def test_macos_hash_fallbacks(self):
        for tool in ("shasum", "openssl"):
            with self.subTest(tool=tool):
                result, installs, _, _ = self.run_case(operating_system="Darwin", hash_tool=tool)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(installs), 2)

    def test_missing_platform_package_stops_before_installation(self):
        result, installs, commands, _ = self.run_case(
            operating_system="Darwin", CODEX_BOOTSTRAP_TEST_MISSING_TARGET="x86_64-apple-darwin")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((installs, commands), ([], []))
        self.assertIn("no x86_64-apple-darwin package", result.stderr)

    def test_windows_is_rejected_before_network(self):
        result, installs, commands, requests = self.run_case(operating_system="MINGW64_NT")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((installs, commands, requests), ([], [], []))

    def test_unexpected_release_redirect_is_rejected(self):
        result, installs, commands, requests = self.run_case(CODEX_BOOTSTRAP_TEST_TAG="https://example.com/rust-v0.166.0+patched.2")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((installs, commands), ([], []))
        self.assertEqual(len(requests), 1)

    def test_help_does_not_install(self):
        result, installs, commands, requests = self.run_case("--help")
        self.assertEqual(result.returncode, 0)
        self.assertEqual((installs, commands, requests), ([], [], []))


if __name__ == "__main__":
    unittest.main()
