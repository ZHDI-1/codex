import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

from platforms import TARGETS, sha256
from publish import assemble


class PublishTest(unittest.TestCase):
    def fixture(self, root):
        candidate, source, inputs = (root / name for name in ("candidate", "source", "inputs"))
        (candidate / "scripts/install").mkdir(parents=True)
        (candidate / "scripts/install/install.sh").write_text("installer\n")
        source.mkdir()
        commit = "a" * 40
        metadata = dict(build_commit=commit, repository="example/codex", version="0.166.0+patched.3")
        (source / "source.json").write_text(json.dumps(metadata))
        (source / "source.patch").write_text("source patch\n")
        for target in TARGETS:
            folder = inputs / f"package-{target}"
            folder.mkdir(parents=True)
            package = folder / f"codex-package-{target}.tar.gz"
            with tarfile.open(package, "w:gz") as archive:
                data = json.dumps(dict(target=target, version=metadata["version"])).encode()
                entry = tarfile.TarInfo("codex-package.json")
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))
            symbols = folder / f"codex-symbols-{target}.tar.gz"
            symbols.write_bytes(b"symbols")
            report = dict(metadata, target=target, native_install_checks=["cli", "daemon"],
                          signing="ad-hoc" if "darwin" in target else "none",
                          artifact_sha256={p.name: sha256(p) for p in (package, symbols)})
            (folder / f"provenance-{target}.json").write_text(json.dumps(report))
        return candidate, source, inputs, commit

    def test_publishes_one_manifest_covering_all_four_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate, source, inputs, commit = self.fixture(root)
            dist = root / "dist"
            assemble(candidate, source, inputs, dist, commit)
            manifest = (dist / "codex-package_SHA256SUMS").read_text()
            self.assertEqual(len(manifest.splitlines()), 4)
            for target in TARGETS:
                name = f"codex-package-{target}.tar.gz"
                self.assertIn(f"{sha256(dist / name)}  {name}", manifest)
            self.assertEqual(json.loads((dist / "provenance.json").read_text())["targets"], list(TARGETS))

    def test_missing_target_or_corrupt_binary_cannot_be_published(self):
        for mode in ("missing", "corrupt", "mixed_commit"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                candidate, source, inputs, commit = self.fixture(root)
                target = TARGETS[-1]
                folder = inputs / f"package-{target}"
                report_file = folder / f"provenance-{target}.json"
                if mode == "missing":
                    report_file.unlink()
                elif mode == "corrupt":
                    (folder / f"codex-package-{target}.tar.gz").write_bytes(b"wrong bytes")
                else:
                    report = json.loads(report_file.read_text())
                    report["build_commit"] = "b" * 40
                    report_file.write_text(json.dumps(report))
                with self.assertRaises((ValueError, FileNotFoundError)):
                    assemble(candidate, source, inputs, root / "dist", commit)
                self.assertFalse((root / "dist").exists())
