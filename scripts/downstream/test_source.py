import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import source


class SourceTest(unittest.TestCase):
    def test_frozen_bundle_recreates_exact_source_on_another_runner(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / "candidate"
            candidate.mkdir()
            env = os.environ.copy()
            for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"):
                env.pop(name, None)
            def git(*args, cwd=candidate):
                return subprocess.check_output(["git", *args], cwd=cwd, env=env, text=True).strip()
            git("init", "-q")
            (candidate / "codex-rs/tui/src").mkdir(parents=True)
            (candidate / "codex-rs/Cargo.toml").write_text('[workspace]\nmembers=["tui"]\n[workspace.package]\nversion="0.0.0"\n')
            (candidate / "codex-rs/tui/Cargo.toml").write_text('[package]\nname="fixture-tui"\nversion.workspace=true\n')
            (candidate / "codex-rs/tui/src/lib.rs").write_text("pub fn fixture() {}\n")
            for path in ("scripts/install/install.sh", "scripts/downstream/recipe.txt", ".github/workflows/fixture.yml"):
                p = candidate / path
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("fixture\n")
            subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=candidate / "codex-rs", env=env, check=True, capture_output=True)
            git("add", ".")
            git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "base")
            parent = git("rev-parse", "HEAD")
            git("tag", "fixture-base")
            receiver = root / "receiver"
            subprocess.run(["git", "clone", "-q", "--depth=1", "--branch", "fixture-base", candidate.as_uri(), str(receiver)], env=env, check=True, capture_output=True)
            manifest = candidate / "codex-rs/Cargo.toml"
            manifest.write_text(manifest.read_text().replace('"0.0.0"', '"0.166.0+patched.3"'))
            (candidate / "downstream-provenance.json").write_text(json.dumps({
                "version": "0.166.0+patched.3", "upstream_commit": parent, "external_dependencies": []}))
            artifacts = root / "source"
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(root / "output")}):
                source.freeze(candidate, artifacts)
            commit = json.loads((artifacts / "source.json").read_text())["build_commit"]
            original_run = source.run
            def local_fetch(*args, **kwargs):
                # The receiving checkout already has the fixture's prerequisite.
                if "https://github.com/openai/codex.git" in args:
                    return subprocess.CompletedProcess(args, 0)
                return original_run(*args, **kwargs)
            restored = root / "restored"
            with patch("source.run", side_effect=local_fetch):
                source.restore(receiver, artifacts, restored, commit)
            self.assertEqual(git("rev-parse", "HEAD", cwd=restored), commit)
            self.assertEqual((restored / "codex-rs/Cargo.lock").read_bytes(), (candidate / "codex-rs/Cargo.lock").read_bytes())
            self.assertEqual(git("status", "--porcelain", cwd=restored), "")
            (artifacts / "source.bundle").write_bytes(b"corrupt")
            with self.assertRaisesRegex(ValueError, "checksum"):
                source.restore(receiver, artifacts, root / "bad", commit)
