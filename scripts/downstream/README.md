# Patched Codex release channel

The default branch maintains our performance and updater patches on top of the tag in
`base-tag`. Every six hours Actions can select the newest upstream stable release,
apply this patch stack, build and test four native Unix packages, and publish them together.
Patch conflicts, dependency drift, test failures, or incomplete packages stop publication.
The last good release remains available. Set the repository variable
`PATCHED_RELEASES_ENABLED=true` to enable scheduled builds; manual dispatch works without it.

The four hosted jobs run concurrently:

| Package target | Native runner | Cargo jobs |
| --- | --- | ---: |
| `x86_64-unknown-linux-musl` | `ubuntu-24.04` | 4 |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | 4 |
| `x86_64-apple-darwin` | `macos-15-intel` | 2 |
| `aarch64-apple-darwin` | `macos-15` | 1 |

The planning job freezes one source commit and lockfile in a Git bundle. Every platform
restores that exact commit; the publisher verifies source IDs, package manifests, native
installation results, and archive checksums before publishing. Any failed platform prevents
a partial release. Windows is not built.

Each platform caches compiled dependencies and downloaded sources, including after a failed
build. Workspace crates are rebuilt when their code or release version changes. Compile,
test, and packaging are separate Actions steps; individual commands also report elapsed
times in the job summary. macOS type-checks the TUI tests before the long executable build,
and uses fewer Cargo jobs to fit the hosted runners' memory. Verified V8 downloads use
the same stable cache directory on every runner.

The recipe needs a user-owned fork, with this branch as its default branch. Enable only
`downstream-release.yml`; the upstream release workflow expects OpenAI runners and secrets.
No PAT is needed: the workflow uses its repository's `GITHUB_TOKEN`. Run numbers are
monotonic within this workflow and become the downstream revision. Do not rename/recreate
the workflow with a lower counter while retaining the same release channel.

Versions are `UPSTREAM+patched.N`, for example `0.166.0+patched.12`. The client and daemon
compile the same `CODEX_RELEASE_REPOSITORY=owner/repository`. Update checks, installer
execution, release notes, and cache files use that channel. Same-upstream revisions
compare numerically; standard SemVer alone would ignore build metadata. The installer
never falls back to an official package. Ordinary builds without this environment
variable keep the official channel.

The release contains the client, the matching daemon-capable executable, code-mode host,
Linux bwrap and platform-specific ripgrep resources, package checksums, source patch, and
build provenance. Each platform has a separate symbols archive for all three first-party
executables. Linux uses `.debug` files; macOS uses matching `.dSYM` bundles. Binaries retain
frame pointers. macOS executables are ad-hoc signed with upstream entitlements and checked
for accidental Homebrew dynamic dependencies; they are not Developer ID notarized.
Native voice runtime packaging is outside this recipe.

For profiling, extract the symbols archive into a `.debug` directory beside the installed
`bin/codex` and `bin/codex-code-mode-host`. The binaries contain GNU debug links to these
matching `.debug` files, and retain their build IDs for tools such as perf and GDB.
On macOS, retain the matching `.dSYM` bundles for LLDB/Instruments; packaging checks their
UUIDs against the shipped executables.

## First cutover

The same bootstrap command supports Linux/macOS on x86_64/ARM64, fresh or existing installs:

```sh
curl -fsSL https://gist.githubusercontent.com/ZHDI-1/9cc96c3eee777b855a3e82d2c73d6f46/raw/install-codex-patched.sh | bash
```

The Gist mirrors `bootstrap.sh` in this directory. Its OS/architecture detection, Rosetta
handling, and SHA-256 fallbacks follow upstream's installer. It verifies the release
installer and confirms the platform package exists before installing both managed roots.
Stock macOS `shasum` works; GNU `sha256sum`, Python, `jq`, and GitHub login are not required.
Pass `bash -s -- --no-restart` to defer the final daemon restart, or `--help` for usage.

Do this after reviewing a successful release. Save the two current selection targets
and their `auto-update-version` markers before changing anything. Close other Codex
clients before restarting the shared daemon; a running client remains on its old binary.

Download `install.sh` and `SHA256SUMS` from the chosen release, inspect the script, and
verify its checksum against that same release. Set `CODEX_NON_INTERACTIVE=1` and run:

```sh
sh ./install.sh
CODEX_INSTALL_DAEMON_ONLY=1 sh ./install.sh
codex app-server daemon restart
codex --version
codex app-server daemon version
```

Both installations select `latest`, so subsequent client updates and daemon auto updates
follow our releases. The daemon-only installer preserves the visible CLI. A pinned
`--release VERSION` installation deliberately disables automatic updates for that package.
`--from-cli` installs are local pins, so use the daemon-only installer for this channel.

This recipe replaces the normal `codex` installation and managed daemon. It does not
change login, model availability, session files, or runtime configuration. Do not rerun
the official installer unless intentionally switching back to official releases.

## Maintenance and rollback

If a patch fails against a new upstream tag, resolve it locally, retest, then move
`base-tag` to the new upstream base and push the updated patch stack to the default branch.
Never skip a failing patch automatically. New releases are tagged with the actual
candidate source commit, rather than the maintenance branch commit. The source patch
and provenance record the upstream and maintenance commits used for the build.
Release commits preserve the maintenance branch's workflow files, allowing the built-in
token to create tags without acquiring permission to introduce new upstream workflows.

To roll back within this channel, rerun the saved installer twice with
`--release OLD_VERSION` (second run with `CODEX_INSTALL_DAEMON_ONLY=1`) and restart the
daemon after closing clients. Pinning prevents an updater from immediately undoing the
rollback. The installer retains its previous selected package; release assets are kept
on GitHub. To return to upstream, use the official installer for both package roots and
restart the daemon. Keep the saved old selections until the first cutover is verified.

Both Linux architectures run scoped history/cache/reflow, daemon install/updater, and
install-context regressions. macOS runs install-context tests and type-checks the TUI test
suite without linking its large test binary. Every platform executes the packaged native
binaries, verifies CLI and daemon installations in temporary homes, and checks its symbols.
The planner also tests bootstrap behavior, source-bundle restoration, release assembly, and
installer regressions. The private long-session tmux benchmark remains a local check;
it is not uploaded to CI, and CI timings must not be compared to that benchmark.
