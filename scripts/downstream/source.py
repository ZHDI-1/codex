#!/usr/bin/env python3
"""Freeze one source commit, then restore that exact commit on every native runner."""

import argparse
import json
import os
import re
from pathlib import Path

from platforms import sha256
from prepare import external_lock, run


def output(**values):
    if filename := os.environ.get("GITHUB_OUTPUT"):
        with open(filename, "a") as stream:
            for key, value in values.items():
                stream.write(f"{key}={value}\n")


def freeze(candidate: Path, artifacts: Path):
    artifacts.mkdir(parents=True, exist_ok=False)
    provenance = json.loads((candidate / "downstream-provenance.json").read_text())
    run("cargo", "update", "--workspace", cwd=candidate / "codex-rs")
    if external_lock(candidate) != [tuple(p) for p in provenance["external_dependencies"]]:
        raise ValueError("external dependencies changed while normalizing workspace versions")
    run("git", "add", "codex-rs", "scripts/install/install.sh", "scripts/downstream",
        ".github/workflows", "downstream-provenance.json", cwd=candidate)
    run("git", "-c", "user.name=github-actions[bot]", "-c",
        "user.email=41898282+github-actions[bot]@users.noreply.github.com", "commit", "-m",
        f"Patched Codex {provenance['version']}", cwd=candidate)
    commit = run("git", "rev-parse", "HEAD", cwd=candidate, capture_output=True).stdout.strip()
    run("git", "bundle", "create", str(artifacts / "source.bundle"), "HEAD",
        f"^{provenance['upstream_commit']}", cwd=candidate)
    patch = run("git", "diff", "--binary", provenance["upstream_commit"], "HEAD",
                cwd=candidate, capture_output=True).stdout
    (artifacts / "source.patch").write_text(patch)
    provenance.update(build_commit=commit, bundle_sha256=sha256(artifacts / "source.bundle"))
    (artifacts / "source.json").write_text(json.dumps(provenance, indent=2) + "\n")
    output(source_sha=commit)


def restore(root: Path, artifacts: Path, candidate: Path, expected_commit: str):
    metadata = json.loads((artifacts / "source.json").read_text())
    if not re.fullmatch("[0-9a-f]{40}", expected_commit) or metadata["build_commit"] != expected_commit:
        raise ValueError("source artifact does not match the planning job's commit")
    parent = metadata["upstream_commit"]
    if not re.fullmatch("[0-9a-f]{40}", parent):
        raise ValueError("invalid upstream commit")
    if sha256(artifacts / "source.bundle") != metadata["bundle_sha256"]:
        raise ValueError("source bundle checksum mismatch")
    run("git", "fetch", "--no-tags", "--depth=1", "https://github.com/openai/codex.git", parent, cwd=root)
    run("git", "bundle", "verify", str(artifacts / "source.bundle"), cwd=root)
    run("git", "fetch", str(artifacts / "source.bundle"), "HEAD", cwd=root)
    fetched = run("git", "rev-parse", "FETCH_HEAD", cwd=root, capture_output=True).stdout.strip()
    if fetched != expected_commit:
        raise ValueError("bundle contains an unexpected source commit")
    run("git", "worktree", "add", "--detach", str(candidate), expected_commit, cwd=root)
    output(candidate=candidate)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("freeze")
    plan.add_argument("candidate", type=Path)
    plan.add_argument("artifacts", type=Path)
    load = commands.add_parser("restore")
    load.add_argument("root", type=Path)
    load.add_argument("artifacts", type=Path)
    load.add_argument("candidate", type=Path)
    load.add_argument("commit")
    args = parser.parse_args()
    if args.command == "freeze":
        freeze(args.candidate.resolve(), args.artifacts.resolve())
    else:
        restore(args.root.resolve(), args.artifacts.resolve(), args.candidate.resolve(), args.commit)
