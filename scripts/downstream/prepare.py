#!/usr/bin/env python3
"""Apply the maintained patch stack to one upstream stable release, without merging branches."""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import tomllib
from pathlib import Path

from configure import configure


def run(*args: str, cwd: Path, **kwargs):
    environment = os.environ.copy()
    for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"):
        environment.pop(name, None)
    return subprocess.run(args, cwd=cwd, env=environment, check=True, text=True, **kwargs)


def external_lock(root: Path) -> list:
    packages = tomllib.loads((root / "codex-rs/Cargo.lock").read_text())["package"]
    return sorted((p["name"], p["version"], p["source"], p.get("checksum", ""))
                  for p in packages if "source" in p)


def prepare(root: Path, candidate: Path, tag: str, repository: str, revision: str) -> None:
    if not re.fullmatch(r"rust-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("only an explicit upstream stable tag is allowed")
    if not re.fullmatch(r"[1-9][0-9]*", revision) or int(revision) > 2**64 - 1:
        raise ValueError("revision must be a positive u64")
    base = (root / "scripts/downstream/base-tag").read_text().strip()
    if not re.fullmatch(r"rust-v[0-9]+\.[0-9]+\.[0-9]+", base):
        raise ValueError("invalid patch-stack base tag")
    version = f"{tag.removeprefix('rust-v')}+patched.{revision}"
    # Validate the distribution before any network operation or worktree creation.
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) or repository == "openai/codex":
        raise ValueError("a downstream owner/repository is required")
    refspecs = sorted({f"refs/tags/{name}:refs/tags/{name}" for name in (base, tag)})
    run("git", "fetch", "--no-tags", "--depth=1", "https://github.com/openai/codex.git",
        *refspecs, cwd=root)
    patch = run("git", "diff", "--binary", base, "HEAD", "--", "codex-rs",
                "scripts/install/install.sh", ":!codex-rs/Cargo.lock", cwd=root, capture_output=True).stdout
    if not patch:
        raise ValueError("empty patch stack")
    run("git", "worktree", "add", "--detach", str(candidate), tag, cwd=root)
    patch_path = candidate.parent / "downstream.patch"
    patch_path.write_text(patch)
    # No fuzzy conflict resolution or silent omission of patches.
    run("git", "apply", "--check", str(patch_path), cwd=candidate)
    run("git", "apply", str(patch_path), cwd=candidate)
    # Keep the maintenance recipe in the resulting release's source tree.
    shutil.copytree(root / "scripts/downstream", candidate / "scripts/downstream", dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns("__pycache__"))
    shutil.copy2(root / ".github/workflows/downstream-release.yml", candidate / ".github/workflows/downstream-release.yml")
    configure(candidate, repository, version)
    provenance = {
        "repository": repository,
        "upstream_tag": tag,
        "upstream_commit": run("git", "rev-parse", f"{tag}^{{commit}}", cwd=root, capture_output=True).stdout.strip(),
        "maintenance_commit": run("git", "rev-parse", "HEAD", cwd=root, capture_output=True).stdout.strip(),
        "patch_sha256": hashlib.sha256(patch.encode()).hexdigest(),
        "version": version,
        "external_dependencies": external_lock(candidate),
    }
    (candidate / "downstream-provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    toolchain = tomllib.loads((candidate / "codex-rs/rust-toolchain.toml").read_text())["toolchain"]["channel"]
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a") as stream:
            stream.write(f"candidate={candidate}\nversion={version}\ntag=rust-v{version}\ntoolchain={toolchain}\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("upstream_tag")
    parser.add_argument("repository")
    parser.add_argument("revision")
    args = parser.parse_args()
    prepare(args.root.resolve(), args.candidate.resolve(), args.upstream_tag, args.repository, args.revision)
