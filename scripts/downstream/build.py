#!/usr/bin/env python3
"""Build and validate a prepared Linux package. This script does not publish or install it."""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

from prepare import external_lock

TARGET = "x86_64-unknown-linux-musl"


def build(root: Path, dist: Path) -> None:
    for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"):
        os.environ.pop(name, None)
    dist.mkdir(parents=True, exist_ok=False)
    rust = root / "codex-rs"
    recipe = Path(__file__).resolve().parents[2]
    provenance = json.loads((root / "downstream-provenance.json").read_text())
    if os.environ.get("CODEX_RELEASE_REPOSITORY") != provenance["repository"]:
        raise ValueError("compiled channel must match the stamped installer")
    os.environ["CODEX_REPO_ROOT"] = str(root)
    sys.path.insert(0, str(root / "scripts"))
    from codex_package.targets import TARGET_SPECS
    from codex_package.v8 import resolve_codex_v8_cargo_env
    os.environ.update(resolve_codex_v8_cargo_env(TARGET_SPECS[TARGET]))

    def run(*args: str, cwd=rust, **kwargs):
        return subprocess.run(args, cwd=cwd, check=True, text=True, **kwargs)

    # Cargo release tags can retain 0.0.0 workspace entries in their lockfile.
    # Normalize only workspace versions and refuse any external dependency drift.
    run("cargo", "update", "--workspace")
    if external_lock(root) != [tuple(p) for p in provenance["external_dependencies"]]:
        raise ValueError("external dependency versions changed; refusing to publish")
    lock_digest = hashlib.sha256((rust / "Cargo.lock").read_bytes()).digest()
    run("git", "add", "codex-rs", "scripts/install/install.sh", "scripts/downstream",
        ".github/workflows", "downstream-provenance.json", cwd=root)
    run("git", "-c", "user.name=github-actions[bot]", "-c",
        "user.email=41898282+github-actions[bot]@users.noreply.github.com",
        "commit", "-m", f"Patched Codex {provenance['version']}", cwd=root)
    commit = run("git", "rev-parse", "HEAD", cwd=root, capture_output=True).stdout.strip()
    os.environ["STABLE_GIT_COMMIT"] = commit
    # Run installer tests against the unstamped recipe, including fork-channel cases.
    run(sys.executable, "-m", "unittest", "discover", "-s", "scripts/install", "-p", "test_install_sh.py", cwd=recipe)
    run(sys.executable, "-m", "unittest", "discover", "-s", "scripts/downstream", "-p", "test_*.py", cwd=recipe)
    stage = dist.parent / "package"
    run(sys.executable, str(root / "scripts/build_codex_package.py"), "--target", TARGET,
        "--cargo-profile", "release", "--strip", "none", "--package-dir", str(stage), cwd=root)
    test = ("cargo", "test", "--locked", "--release", "--target", TARGET)
    run(*test, "-p", "codex-install-context", "--lib")
    run(*test, "-p", "codex-app-server-daemon", "--lib", "--",
        "managed_install", "prepare_install", "update_loop")
    run(*test, "-p", "codex-tui", "--lib", "--", "app_server_session::history::tests",
        "history_render::tests", "render::highlight_cache::tests", "resize_reflow",
        "insert_history", "update_versions", "update_action", "updates_cache")
    if hashlib.sha256((rust / "Cargo.lock").read_bytes()).digest() != lock_digest:
        raise ValueError("package build changed Cargo.lock")
    if run("git", "status", "--porcelain", "--untracked-files=no", cwd=root, capture_output=True).stdout:
        raise ValueError("build modified the committed source")
    version = run(str(stage / "bin/codex"), "--version", capture_output=True).stdout.strip()
    if version != f"codex-cli {provenance['version']}":
        raise ValueError(f"package version mismatch: {version}")
    debug = dist.parent / "debug-symbols"
    debug.mkdir()
    for name in ("codex", "codex-code-mode-host"):
        binary = stage / "bin" / name
        symbols = debug / f"{name}.debug"
        run("objcopy", "--only-keep-debug", str(binary), str(symbols))
        run("strip", "--strip-unneeded", str(binary))
        run("objcopy", f"--add-gnu-debuglink={symbols}", str(binary))
    for folder, name in [(stage, f"codex-package-{TARGET}.tar.gz"), (debug, f"codex-symbols-{TARGET}.tar.gz")]:
        with tarfile.open(dist / name, "w:gz") as archive:
            for path in sorted(folder.iterdir()):
                archive.add(path, arcname=path.name)
    package_name = f"codex-package-{TARGET}.tar.gz"
    digest = hashlib.sha256((dist / package_name).read_bytes()).hexdigest()
    (dist / "codex-package_SHA256SUMS").write_text(f"{digest}  {package_name}\n")
    shutil.copy2(root / "scripts/install/install.sh", dist / "install.sh")
    # Exercise the real stripped package through the installer in temporary homes.
    # Network downloads are replaced with the exact files destined for this release.
    sys.path.insert(0, str(recipe / "scripts/install"))
    import test_install_sh as installer_test
    assets = [dist / package_name, dist / "codex-package_SHA256SUMS"]
    metadata = json.dumps({"tag_name": f"rust-v{provenance['version']}", "assets": [
        {"name": p.name, "digest": f"sha256:{hashlib.sha256(p.read_bytes()).hexdigest()}"}
        for p in assets
    ]})
    for daemon_only in (False, True):
        with tempfile.TemporaryDirectory(prefix="codex-channel-smoke-") as temporary:
            result, requests = installer_test.run_installer_in(
                Path(temporary), "latest", metadata_json=metadata,
                archive_path=dist / package_name, checksum_path=dist / "codex-package_SHA256SUMS",
                daemon_only=daemon_only, use_mirror=True, installer_path=dist / "install.sh",
            )
            if result.returncode or not all(f"/{provenance['repository']}/" in url for url in requests):
                raise ValueError(f"installer smoke test failed: {result.stdout}\n{result.stderr}")
    patch = run("git", "diff", "--binary", provenance["upstream_tag"], "HEAD", cwd=root, capture_output=True).stdout
    (dist / "source.patch").write_text(patch)
    provenance.update(build_commit=commit, target=TARGET, frame_pointers=True, debug_info="limited")
    (dist / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    checksums = "".join(f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in sorted(dist.iterdir()))
    (dist / "SHA256SUMS").write_text(checksums)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("dist", type=Path)
    args = parser.parse_args()
    build(args.candidate.resolve(), args.dist.resolve())
