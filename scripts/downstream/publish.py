#!/usr/bin/env python3
"""Validate every native job's artifacts and assemble one complete release directory."""

import argparse
import json
from pathlib import Path
import shutil
import tarfile

from platforms import TARGETS, sha256


def write_release_metadata(dist, version):
    # Keep the GitHub API's shape so both the shell installer and TUI can read it
    # through github.com release downloads without consuming anonymous API quota.
    names = [f"codex-package-{target}.tar.gz" for target in TARGETS]
    names.append("codex-package_SHA256SUMS")
    metadata = {
        "tag_name": f"rust-v{version}",
        "assets": [{"name": name, "digest": f"sha256:{sha256(dist / name)}"}
                   for name in sorted(names)],
    }
    (dist / "release.json").write_text(json.dumps(metadata, indent=2) + "\n")


def assemble(candidate, source, inputs, dist, expected_commit):
    metadata = json.loads((source / "source.json").read_text())
    if metadata["build_commit"] != expected_commit:
        raise ValueError("publishing source does not match the planning job")
    # Validate all four jobs before creating anything that can be published.
    reports = {}
    for target in TARGETS:
        folder = inputs / f"package-{target}"
        report = json.loads((folder / f"provenance-{target}.json").read_text())
        for key, value in (("target", target), ("build_commit", expected_commit),
                           ("version", metadata["version"]), ("repository", metadata["repository"])):
            if report[key] != value:
                raise ValueError(f"{target}: mismatched {key}")
        if report.get("native_install_checks") != ["cli", "daemon"]:
            raise ValueError(f"{target}: native installation checks were not completed")
        expected_names = {f"codex-{kind}-{target}.tar.gz" for kind in ("package", "symbols")}
        if set(report["artifact_sha256"]) != expected_names:
            raise ValueError(f"{target}: incomplete artifact list")
        for name, digest in report["artifact_sha256"].items():
            if sha256(folder / name) != digest:
                raise ValueError(f"{target}: checksum mismatch for {name}")
        with tarfile.open(folder / f"codex-package-{target}.tar.gz") as archive:
            entry = archive.extractfile("codex-package.json")
            if entry is None:
                raise ValueError(f"{target}: missing package manifest")
            package = json.load(entry)
        if package["target"] != target or package["version"] != metadata["version"]:
            raise ValueError(f"{target}: package manifest does not match the release")
        reports[target] = report
    dist.mkdir(parents=True, exist_ok=False)
    for target in TARGETS:
        folder = inputs / f"package-{target}"
        for name in (*reports[target]["artifact_sha256"], f"provenance-{target}.json"):
            shutil.copy2(folder / name, dist / name)
    packages = sorted(dist.glob("codex-package-*.tar.gz"))
    (dist / "codex-package_SHA256SUMS").write_text(
        "".join(f"{sha256(p)}  {p.name}\n" for p in packages))
    shutil.copy2(candidate / "scripts/install/install.sh", dist / "install.sh")
    shutil.copy2(source / "source.patch", dist / "source.patch")
    metadata["targets"] = list(TARGETS)
    metadata["signing"] = {target: reports[target]["signing"] for target in TARGETS}
    (dist / "provenance.json").write_text(json.dumps(metadata, indent=2) + "\n")
    write_release_metadata(dist, metadata["version"])
    (dist / "SHA256SUMS").write_text(
        "".join(f"{sha256(p)}  {p.name}\n" for p in sorted(dist.iterdir())))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("source", type=Path)
    parser.add_argument("inputs", type=Path)
    parser.add_argument("dist", type=Path)
    parser.add_argument("commit")
    args = parser.parse_args()
    assemble(args.candidate, args.source, args.inputs, args.dist, args.commit)
