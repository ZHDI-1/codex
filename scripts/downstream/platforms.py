"""The complete supported release matrix; publication requires every target."""

TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)
BINARIES = ("codex", "codex-code-mode-host", "codex-exec-server")


def sha256(path):
    import hashlib
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()
