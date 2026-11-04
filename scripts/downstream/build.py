#!/usr/bin/env python3
"""Compile, test, and package one native Unix target from the frozen source commit."""

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time

from platforms import BINARIES, TARGETS, sha256


@contextmanager
def timed(command):
    started = time.monotonic()
    print(f"Starting: {command}", flush=True)
    try:
        yield
    finally:
        elapsed = time.monotonic() - started
        print(f"Elapsed {elapsed:.1f}s: {command}", flush=True)
        if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
            escaped = command.replace("|", "&#124;").replace("`", "&#96;")
            with open(summary, "a") as stream:
                stream.write(f"\n{elapsed:.1f}s — `{escaped}`\n")


def run(*args, cwd, **kwargs):
    with timed(shlex.join(map(str, args))):
        return subprocess.run(list(map(str, args)), cwd=cwd, check=True, text=True, **kwargs)


def validate_native_target(target):
    import platform
    machine = {"arm64": "aarch64", "amd64": "x86_64"}.get(platform.machine(), platform.machine())
    expected_os = "Darwin" if target.endswith("apple-darwin") else "Linux"
    if platform.system() != expected_os or target.split("-", 1)[0] != machine:
        raise ValueError(f"{target} must be built and tested on its native architecture")


def verify_dsym(binary, symbols, *, cwd):
    executable_uuids = run("xcrun", "dwarfdump", "--uuid", binary, cwd=cwd, capture_output=True).stdout
    symbol_uuids = run("xcrun", "dwarfdump", "--uuid", symbols, cwd=cwd, capture_output=True).stdout
    def uuids(value):
        return {line.split()[1] for line in value.splitlines() if line.startswith("UUID:")}
    if not uuids(executable_uuids) or uuids(executable_uuids) != uuids(symbol_uuids):
        raise ValueError(f"debug symbols do not match {binary}")


def build(root, dist, target, phase):
    validate_native_target(target)
    for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"):
        os.environ.pop(name, None)
    rust = root / "codex-rs"
    recipe = Path(__file__).resolve().parents[2]
    provenance = json.loads((root / "downstream-provenance.json").read_text())
    if os.environ.get("CODEX_RELEASE_REPOSITORY") != provenance["repository"]:
        raise ValueError("compiled channel must match the stamped installer")
    commit = run("git", "rev-parse", "HEAD", cwd=root, capture_output=True).stdout.strip()
    if os.environ.get("STABLE_GIT_COMMIT") != commit:
        raise ValueError("build must use the exact frozen source commit")
    lock_digest = sha256(rust / "Cargo.lock")
    os.environ["CODEX_REPO_ROOT"] = str(root)
    sys.path.insert(0, str(root / "scripts"))
    from codex_package.targets import TARGET_SPECS
    from codex_package.v8 import resolve_codex_v8_cargo_env
    with timed("Resolve and verify V8 artifacts"):
        os.environ.update(resolve_codex_v8_cargo_env(
            TARGET_SPECS[target], cache_root=Path("/tmp/codex-package")))
    output = Path(os.environ.get("CARGO_TARGET_DIR", rust / "target"))
    if not output.is_absolute():
        output = rust / output
    output = output / target / "release"
    linux = target.endswith("linux-musl")
    stage = dist.parent / "package"

    if phase in ("all", "check") and not linux:
        # Check before code generation so compiler failures do not waste a full build.
        run("cargo", "check", "--locked", "--release", "--target", target,
            "-p", "codex-tui", "--tests", cwd=rust)

    if phase in ("all", "compile"):
        command = ["cargo", "build", "--locked", "--release", "--target", target]
        for binary in (*BINARIES, *(("bwrap",) if linux else ())):
            command += ["--bin", binary]
        run(*command, cwd=rust)

    if phase in ("all", "test"):
        test = ["cargo", "test", "--locked", "--release", "--target", target]
        run(*test, "-p", "codex-install-context", "--lib", cwd=rust)
        # Keep the large transcript test binary on the 16 GiB Linux runners.
        # Macs type-check that suite first and run native install/signature checks below.
        if linux:
            run(*test, "-p", "codex-app-server-daemon", "--lib", "--",
                "managed_install", "prepare_install", "update_loop", cwd=rust)
            run(*test, "-p", "codex-tui", "--lib", "--",
                "app_server_session::history::tests", "history_render::tests",
                "render::highlight_cache::tests", "resize_reflow", "insert_history",
                "prompt_edit", "backtrack", "native_history",
                "update_versions", "update_action", "updates_cache", cwd=rust)

    if phase in ("all", "package"):
        dist.mkdir(parents=True, exist_ok=False)
        command = [sys.executable, str(root / "scripts/build_codex_package.py"), "--target", target,
                   "--cargo-profile", "release", "--strip", "none", "--package-dir", str(stage),
                   "--entrypoint-bin", str(output / "codex"),
                   "--code-mode-host-bin", str(output / "codex-code-mode-host"),
                   "--exec-server-bin", str(output / "codex-exec-server")]
        if linux:
            command += ["--bwrap-bin", str(output / "bwrap")]
        run(*command, cwd=root)
        debug = dist.parent / "debug-symbols"
        debug.mkdir()
        for name in BINARIES:
            binary = stage / "bin" / name
            if linux:
                symbols = debug / f"{name}.debug"
                run("objcopy", "--only-keep-debug", binary, symbols, cwd=rust)
                run("strip", "--strip-unneeded", binary, cwd=rust)
                run("objcopy", f"--add-gnu-debuglink={symbols}", binary, cwd=rust)
            else:
                symbols = debug / f"{name}.dSYM"
                # Use the original unstripped Cargo file for the DWARF map.
                run("xcrun", "dsymutil", output / name, "-o", symbols, cwd=rust)
                run("xcrun", "strip", "-S", "-x", binary, cwd=rust)
                entitlements = root / ".github/scripts/macos-signing" / f"{name}.entitlements.plist"
                run("codesign", "--force", "--sign", "-", "--timestamp=none",
                    "--entitlements", entitlements, binary, cwd=rust)
                run("codesign", "--verify", "--strict", binary, cwd=rust)
                verify_dsym(binary, symbols, cwd=rust)
            run(binary, "--help", cwd=rust, stdout=subprocess.DEVNULL)
        # Ensure no Homebrew OpenSSL dependency is required on a destination Mac.
        if not linux:
            for name in BINARIES:
                libraries = run("otool", "-L", stage / "bin" / name,
                                cwd=rust, capture_output=True).stdout
                if "/opt/homebrew/" in libraries or "/usr/local/" in libraries:
                    raise ValueError(f"package has a machine-local dynamic dependency: {libraries}")
        version = run(stage / "bin/codex", "--version", cwd=rust, capture_output=True).stdout.strip()
        if version != f"codex-cli {provenance['version']}":
            raise ValueError(f"package version mismatch: {version}")
        for folder, name in [(stage, f"codex-package-{target}.tar.gz"),
                             (debug, f"codex-symbols-{target}.tar.gz")]:
            with timed(f"Archive {name}"), tarfile.open(dist / name, "w:gz", compresslevel=3) as archive:
                for path in sorted(folder.iterdir()):
                    archive.add(path, arcname=path.name)
        package_name = f"codex-package-{target}.tar.gz"
        checksum = dist / "codex-package_SHA256SUMS"
        checksum.write_text(f"{sha256(dist / package_name)}  {package_name}\n")
        sys.path.insert(0, str(recipe / "scripts/install"))
        import test_install_sh as installer_test
        metadata = json.dumps({"tag_name": f"rust-v{provenance['version']}", "assets": [
            {"name": p.name, "digest": f"sha256:{sha256(p)}"}
            for p in (dist / package_name, checksum)
        ]})
        for daemon_only in (False, True):
            with tempfile.TemporaryDirectory(prefix="codex-channel-smoke-") as temporary:
                result, requests = installer_test.run_installer_in(
                    Path(temporary), "latest", metadata_json=metadata,
                    archive_path=dist / package_name, checksum_path=checksum,
                    daemon_only=daemon_only, use_mirror=True,
                    installer_path=root / "scripts/install/install.sh")
                if result.returncode or not all(f"/{provenance['repository']}/" in url for url in requests):
                    raise ValueError(f"installer smoke test failed: {result.stdout}\n{result.stderr}")
        checksum.unlink()  # The publishing job creates the combined four-target manifest.
        artifacts = {p.name: sha256(p) for p in sorted(dist.glob("*.tar.gz"))}
        provenance.update(build_commit=commit, target=target, frame_pointers=True,
                          debug_info="limited", signing="none" if linux else "ad-hoc",
                          native_install_checks=["cli", "daemon"], artifact_sha256=artifacts)
        (dist / f"provenance-{target}.json").write_text(json.dumps(provenance, indent=2) + "\n")

    if sha256(rust / "Cargo.lock") != lock_digest:
        raise ValueError("build changed the frozen Cargo.lock")
    if run("git", "status", "--porcelain", "--untracked-files=no", cwd=root, capture_output=True).stdout:
        raise ValueError("build modified the committed source")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("dist", type=Path)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--phase", choices=("all", "check", "compile", "test", "package"), default="all")
    args = parser.parse_args()
    build(args.candidate.resolve(), args.dist.resolve(), args.target, args.phase)
