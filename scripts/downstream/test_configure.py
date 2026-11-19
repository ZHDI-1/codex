import tempfile
import unittest
from pathlib import Path

from configure import configure


class ConfigureTest(unittest.TestCase):
    def fixture(self, root):
        (root / "codex-rs").mkdir()
        (root / "scripts/install").mkdir(parents=True)
        (root / "codex-rs/Cargo.toml").write_text('[workspace.package]\nversion = "0.166.0"\nedition = "2024"\n')
        (root / "scripts/install/install.sh").write_text('GITHUB_RELEASE_REPOSITORY="openai/codex"\n')

    def test_stamps_matching_package_and_installer(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            configure(root, "example/codex", "0.166.0+patched.12")
            self.assertIn('version = "0.166.0+patched.12"', (root / "codex-rs/Cargo.toml").read_text())
            self.assertIn('GITHUB_RELEASE_REPOSITORY="example/codex"', (root / "scripts/install/install.sh").read_text())

    def test_rejects_malformed_channel_before_changing_files(self):
        for repository in ("openai/codex", "example/a;whoami", "example/../codex", "example/.."):
            with self.subTest(repository=repository), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.fixture(root)
                with self.assertRaises(ValueError):
                    configure(root, repository, "0.166.0+patched.12")
                self.assertIn('version = "0.166.0"', (root / "codex-rs/Cargo.toml").read_text())

    def test_missing_installer_marker_does_not_partially_stamp_version(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            (root / "scripts/install/install.sh").write_text("changed upstream\n")
            with self.assertRaises(ValueError):
                configure(root, "example/codex", "0.166.0+patched.12")
            self.assertIn('version = "0.166.0"', (root / "codex-rs/Cargo.toml").read_text())
