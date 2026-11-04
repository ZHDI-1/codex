#!/usr/bin/env python3
"""Stamp a downstream candidate; upstream defaults remain unchanged in the patch stack."""

import argparse
import re
from pathlib import Path


def configure(root: Path, repository: str, version: str) -> None:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("expected an owner/repository pair")
    if repository == "openai/codex" or any(p in (".", "..") for p in repository.split("/")):
        raise ValueError("expected a downstream repository")
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\+patched\.[1-9][0-9]*", version):
        raise ValueError("expected x.y.z+patched.N")
    cargo = root / "codex-rs/Cargo.toml"
    contents, count = re.subn(
        r'(\[workspace\.package\]\s*\n(?:[^\[]*?\n)?version\s*=\s*)"[^"]+"',
        lambda m: f'{m[1]}"{version}"', cargo.read_text(), count=1,
    )
    if count != 1:
        raise ValueError("workspace version declaration changed; review before releasing")
    installer = root / "scripts/install/install.sh"
    source = installer.read_text()
    marker = 'GITHUB_RELEASE_REPOSITORY="openai/codex"'
    if source.count(marker) != 1:
        raise ValueError("installer repository declaration changed; review before releasing")
    cargo.write_text(contents)
    installer.write_text(source.replace(marker, f'GITHUB_RELEASE_REPOSITORY="{repository}"'))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("repository")
    parser.add_argument("version")
    args = parser.parse_args()
    configure(args.root, args.repository, args.version)
