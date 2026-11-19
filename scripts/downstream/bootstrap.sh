#!/usr/bin/env bash
# Install/update ZHDI-1/codex for Linux/macOS, x86_64/ARM64. No gh/jq/Python required.
# SPDX-License-Identifier: Apache-2.0
# Platform/checksum logic adapted from OpenAI Codex scripts/install/install.sh:
# https://github.com/openai/codex/blob/rust-v0.166.0/scripts/install/install.sh
# Both managed packages follow this fork's latest release after installation.
set -euo pipefail

codex_bootstrap_tmp=''

cleanup() {
  if [[ -n "$codex_bootstrap_tmp" ]]; then
    rm -rf -- "$codex_bootstrap_tmp"
  fi
}

fail() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

download() {
  curl -fsSL --retry 3 --connect-timeout 15 --max-time 180 "$@"
}

# Equivalent to the upstream installer fallback; stock macOS provides shasum.
file_sha256() {
  local codex_hash_path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$codex_hash_path" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$codex_hash_path" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$codex_hash_path" | sed 's/^.*= //'
  else
    fail 'sha256sum, shasum, or openssl is required to verify downloads.'
  fi
}

main() {
  local codex_restart=1
  while (( $# )); do
    case "$1" in
      --no-restart) codex_restart=0 ;;
      -h|--help)
        printf '%s\n' \
          'Usage: bash install-codex-patched.sh [--no-restart]' \
          'Installs/updates both the CLI and daemon from ZHDI-1/codex.' \
          'Default: restart/start the daemon; attached clients will disconnect.' \
          '--no-restart: install both packages, but defer the explicit daemon restart.' \
          'Platforms: Linux/macOS, x86_64/ARM64 (including Rosetta). No GitHub login required.'
        return 0
        ;;
      *) fail "Unknown option: $1 (use --help)" ;;
    esac
    shift
  done

  local codex_tool
  for codex_tool in curl uname awk mktemp sh; do
    command -v "$codex_tool" >/dev/null 2>&1 || fail "Required command missing: $codex_tool"
  done
  # Same OS/architecture aliases and Rosetta preference as the upstream installer.
  local codex_os codex_arch codex_target
  case "$(uname -s)" in
    Darwin) codex_os=darwin ;;
    Linux) codex_os=linux ;;
    *) fail 'Supported operating systems: macOS and Linux.' ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) codex_arch=x86_64 ;;
    arm64|aarch64) codex_arch=aarch64 ;;
    *) fail "Unsupported architecture: $(uname -m)" ;;
  esac
  if [[ "$codex_os" == darwin && "$codex_arch" == x86_64 ]] &&
    [[ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" == 1 ]]; then
    codex_arch=aarch64
  fi
  if [[ "$codex_os" == darwin ]]; then
    codex_target="$codex_arch-apple-darwin"
  else
    codex_target="$codex_arch-unknown-linux-musl"
  fi
  if ! command -v sha256sum >/dev/null 2>&1 &&
     ! command -v shasum >/dev/null 2>&1 &&
     ! command -v openssl >/dev/null 2>&1; then
    fail 'sha256sum, shasum, or openssl is required to verify downloads.'
  fi

  local codex_repo_url='https://github.com/ZHDI-1/codex'
  local codex_tag_url codex_tag codex_asset_url codex_expected_sha codex_actual_sha
  printf 'Resolving the latest patched Codex release...\n'
  # Resolve once so the installer and its checksum manifest come from the same tag.
  codex_tag_url=$(download -o /dev/null -w '%{url_effective}' "$codex_repo_url/releases/latest")
  [[ "$codex_tag_url" == "$codex_repo_url/releases/tag/"* ]] ||
    fail "Unexpected release redirect: $codex_tag_url"
  codex_tag=${codex_tag_url#"$codex_repo_url/releases/tag/"}
  [[ "$codex_tag" =~ ^rust-v[0-9]+\.[0-9]+\.[0-9]+(%2[Bb]|\+)patched\.[1-9][0-9]*$ ]] ||
    fail "Unexpected release tag: $codex_tag"
  codex_asset_url="$codex_repo_url/releases/download/$codex_tag"

  codex_bootstrap_tmp=$(mktemp -d "${TMPDIR:-/tmp}/codex-patched.XXXXXXXXXX")
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  download "$codex_asset_url/install.sh" -o "$codex_bootstrap_tmp/install.sh"
  download "$codex_asset_url/SHA256SUMS" -o "$codex_bootstrap_tmp/SHA256SUMS"
  codex_expected_sha=$(awk '$2 == "install.sh" { print tolower($1) }' "$codex_bootstrap_tmp/SHA256SUMS")
  [[ "$codex_expected_sha" =~ ^[[:xdigit:]]{64}$ ]] || fail 'Installer checksum is missing or malformed.'
  codex_actual_sha=$(file_sha256 "$codex_bootstrap_tmp/install.sh")
  [[ "$codex_actual_sha" == "$codex_expected_sha" ]] || fail 'Installer checksum mismatch; nothing was installed.'

  # Fail before installing anything if the current release lacks this platform.
  local codex_package="codex-package-$codex_target.tar.gz"
  awk -v asset="$codex_package" '$2 == asset && length($1) == 64 && $1 !~ /[^0-9a-fA-F]/ { found=1 } END { exit !found }' \
    "$codex_bootstrap_tmp/SHA256SUMS" ||
    fail "Latest fork release has no $codex_target package yet; nothing was installed."
  printf 'Verified installer from %s for %s\n' "$codex_tag_url" "$codex_target"
  if (( codex_restart )); then
    printf 'The shared daemon will restart after installation; attached Codex clients will disconnect.\n'
  fi
  printf 'Installing the CLI from the fork latest channel...\n'
  CODEX_RELEASE=latest CODEX_NON_INTERACTIVE=1 CODEX_INSTALL_DAEMON_ONLY=0 \
    sh "$codex_bootstrap_tmp/install.sh" </dev/null
  printf 'Installing the managed daemon from the fork latest channel...\n'
  CODEX_RELEASE=latest CODEX_NON_INTERACTIVE=1 CODEX_INSTALL_DAEMON_ONLY=1 \
    sh "$codex_bootstrap_tmp/install.sh" </dev/null

  # The installer may have added PATH to a startup file, not to this parent shell.
  # Always invoke the newly installed binary, even if another codex shadows it.
  local codex_bin_dir="${CODEX_INSTALL_DIR:-$HOME/.local/bin}"
  local codex_bin="$codex_bin_dir/codex"
  [[ -x "$codex_bin" ]] || fail "Installed binary was not found at $codex_bin"
  if (( codex_restart )); then
    "$codex_bin" app-server daemon restart </dev/null
  else
    printf 'Daemon restart deferred. When ready, run: %q app-server daemon restart\n' "$codex_bin"
  fi
  "$codex_bin" --version
  if (( codex_restart )); then
    "$codex_bin" app-server daemon version </dev/null
  fi
  printf '\nInstalled command: %s\n' "$codex_bin"
  printf 'Reopen Codex to use the new binary. Future updates: codex update\n'
  case ":$PATH:" in
    *":$codex_bin_dir:"*) ;;
    *) printf "For this terminal: export PATH=%q:\"\$PATH\"\n" "$codex_bin_dir" ;;
  esac
}

main "$@"
