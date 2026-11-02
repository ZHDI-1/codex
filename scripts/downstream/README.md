# Patched Codex release channel

The default branch maintains our performance and updater patches on top of the tag in
`base-tag`. Every six hours Actions can select the newest upstream stable release,
apply this patch stack, build and test an x86_64 Linux musl package, and publish it.
Patch conflicts, dependency drift, test failures, or incomplete packages stop publication.
The last good release remains available. Set the repository variable
`PATCHED_RELEASES_ENABLED=true` to enable scheduled builds; manual dispatch works without it.

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
bwrap and ripgrep resources, package checksums, source patch, build provenance, and a
separate symbols archive containing `codex.debug`. Binaries retain frame pointers.
Linux x86_64 is the only initial target; do not install this channel on other platforms.
Native voice runtime packaging and signed macOS/Windows builds are outside this recipe.

For profiling, extract the symbols archive into a `.debug` directory beside the installed
`bin/codex` and `bin/codex-code-mode-host`. The binaries contain GNU debug links to these
matching `.debug` files, and retain their build IDs for tools such as perf and GDB.

## First cutover

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
codex app-server daemon status
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

To roll back within this channel, rerun the saved installer twice with
`--release OLD_VERSION` (second run with `CODEX_INSTALL_DAEMON_ONLY=1`) and restart the
daemon after closing clients. Pinning prevents an updater from immediately undoing the
rollback. The installer retains its previous selected package; release assets are kept
on GitHub. To return to upstream, use the official installer for both package roots and
restart the daemon. Keep the saved old selections until the first cutover is verified.

Actions runs scoped history/cache/reflow, install-context, daemon install/updater, and
installer regressions. The private long-session tmux benchmark remains a local check;
it is not uploaded to CI, and CI timings must not be compared to that benchmark.
