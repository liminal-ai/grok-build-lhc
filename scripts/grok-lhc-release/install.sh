#!/bin/sh
# grok-build-lhc installer: the one owner of the managed store, checksums,
# receipts, and activation. Modes:
#   --asset-dir DIR   install a local candidate (release lane, offline)
#   --download        fetch the release assets from GitHub Releases (or
#                     $GROK_LHC_RELEASE_BASE/download/... for a local test server)
# Layout: <store>/versions/<release>/{bin/grok,release-manifest.json},
# <store>/current -> versions/<release>, <prefix>/bin/<name> -> current/bin/grok,
# receipts .grok-lhc-managed, installed-name, installed-version, installed-prefix.
# Asset convention: grok-<release>-<os>-<arch>, os linux|darwin|windows,
# arch x86_64|aarch64. Never touches ~/.grok.
set -eu

STORE="${GROK_LHC_INSTALL_ROOT:-${XDG_DATA_HOME:-${HOME}/.local/share}/grok-lhc}"
PREFIX="${GROK_LHC_PREFIX:-}"
VERSION="${GROK_LHC_VERSION:-}"
ASSET_DIR="${GROK_LHC_ASSET_DIR:-}"
NAME="${GROK_LHC_NAME:-}"
PLATFORM="${GROK_LHC_PLATFORM:-}"
DOWNLOAD=0
UNINSTALL=0
RELEASE_BASE="${GROK_LHC_RELEASE_BASE:-}"
if [ -n "$RELEASE_BASE" ]; then
  DOWNLOAD_BASE="${RELEASE_BASE%/}/download"
  LATEST_URL="${RELEASE_BASE%/}/latest"
else
  DOWNLOAD_BASE="https://github.com/liminal-ai/grok-build-lhc/releases/download"
  LATEST_URL="https://api.github.com/repos/liminal-ai/grok-build-lhc/releases/latest"
fi

die() { printf 'grok-lhc installer: %s\n' "$*" >&2; exit 1; }
usage() {
  printf '%s\n' 'Usage: install.sh (--asset-dir DIR | --download) [--version RELEASE] [--name NAME] [--prefix DIR] [--install-root DIR] [--platform OS-ARCH]' \
    '       install.sh --uninstall [--name NAME] [--prefix DIR] [--install-root DIR]'
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) VERSION=$2; shift 2 ;;
    --name) NAME=$2; shift 2 ;;
    --prefix) PREFIX=$2; shift 2 ;;
    --install-root) STORE=$2; shift 2 ;;
    --asset-dir) ASSET_DIR=$2; shift 2 ;;
    --platform) PLATFORM=$2; shift 2 ;;
    --download) DOWNLOAD=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

case "$STORE" in ''|/|"$HOME") die "refusing unsafe install root: $STORE" ;; esac

# Receipts win for an existing store: the recorded command name and prefix are
# reused unless the caller names the same ones explicitly.
if [ -z "$PREFIX" ] && [ -f "$STORE/installed-prefix" ]; then
  PREFIX=$(cat "$STORE/installed-prefix")
fi
[ -n "$PREFIX" ] || PREFIX="${HOME}/.local"

if [ "$UNINSTALL" -eq 1 ]; then
  [ -f "$STORE/.grok-lhc-managed" ] || die "$STORE is not managed by this installer"
  [ -f "$STORE/installed-name" ] || die "$STORE is missing its installed command receipt"
  installed_name=$(cat "$STORE/installed-name")
  if [ -n "$NAME" ] && [ "$NAME" != "$installed_name" ]; then
    die "installed command is $installed_name, not $NAME"
  fi
  NAME=$installed_name
  case "$NAME" in ''|*/*) die "invalid installed command receipt" ;; esac
  LINK="$PREFIX/bin/$NAME"
  if [ -L "$LINK" ]; then
    case "$(readlink "$LINK")" in "$STORE"/*) rm -f "$LINK" ;; *) die "$LINK is not managed by this installer" ;; esac
  elif [ -e "$LINK" ]; then
    die "$LINK is not a managed symlink"
  fi
  rm -rf "$STORE"
  printf 'Removed Grok-LHC command and managed packages; user configuration and LHC archives were preserved.\n'
  exit 0
fi

if [ -e "$STORE" ] && [ ! -f "$STORE/.grok-lhc-managed" ]; then
  die "$STORE already exists and is not managed by this installer"
fi
if [ -f "$STORE/installed-name" ]; then
  installed_name=$(cat "$STORE/installed-name")
  if [ -n "$NAME" ] && [ "$NAME" != "$installed_name" ]; then
    die "managed store is installed as $installed_name; use that name"
  fi
  NAME=$installed_name
fi
[ -n "$NAME" ] || NAME=grok-lhc
case "$NAME" in ''|*/*) die "--name must be a command name" ;; esac
LINK="$PREFIX/bin/$NAME"

if [ "$DOWNLOAD" -eq 1 ] && [ -n "$ASSET_DIR" ]; then
  die "--download and --asset-dir are exclusive"
fi
if [ "$DOWNLOAD" -eq 0 ] && [ -z "$ASSET_DIR" ]; then
  die "--asset-dir DIR or --download is required"
fi

fetch() { # url dest
  command -v curl >/dev/null 2>&1 || die "curl is required for --download"
  curl -fsSL "$1" -o "$2" || die "download failed: $1"
}

if [ "$DOWNLOAD" -eq 1 ] && [ -z "$VERSION" ]; then
  latest_json=$(mktemp "${TMPDIR:-/tmp}/grok-lhc-latest.XXXXXX")
  fetch "$LATEST_URL" "$latest_json"
  VERSION=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' "$latest_json" | head -n 1)
  rm -f "$latest_json"
  [ -n "$VERSION" ] || die "could not resolve the latest release from $LATEST_URL"
fi
[ -n "$VERSION" ] || die "--version is required with --asset-dir"
printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-lhc\.[0-9]+)?$' || die "invalid release: $VERSION (expected <major>.<minor>.<patch>[-lhc.<n>])"

if [ -z "$PLATFORM" ]; then
  case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    *) die "unsupported OS: $(uname -s) (use --platform OS-ARCH)" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    arm64|aarch64) arch=aarch64 ;;
    *) die "unsupported architecture: $(uname -m) (use --platform OS-ARCH)" ;;
  esac
  PLATFORM="$os-$arch"
fi
case "$PLATFORM" in
  linux-x86_64|linux-aarch64|darwin-x86_64|darwin-aarch64) ;;
  *) die "unsupported platform: $PLATFORM" ;;
esac
ASSET="grok-${VERSION}-${PLATFORM}"

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "sha256sum or shasum is required"
fi

CLEANUP_DIR=""
if [ "$DOWNLOAD" -eq 1 ]; then
  CLEANUP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/grok-lhc-download.XXXXXX")
  trap 'rm -rf "$CLEANUP_DIR"' EXIT
  ASSET_DIR=$CLEANUP_DIR
  for f in SHA256SUMS release-manifest.json "$ASSET"; do
    fetch "$DOWNLOAD_BASE/v$VERSION/$f" "$ASSET_DIR/$f"
  done
fi

[ -f "$ASSET_DIR/$ASSET" ] || die "release is missing $ASSET"
[ -f "$ASSET_DIR/SHA256SUMS" ] || die "release is missing SHA256SUMS"
[ -f "$ASSET_DIR/release-manifest.json" ] || die "release is missing release-manifest.json"
expected=$(awk -v name="$ASSET" '$2 == name { print $1 }' "$ASSET_DIR/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS does not list $ASSET"
actual=$(sha256 "$ASSET_DIR/$ASSET")
[ "$actual" = "$expected" ] || die "checksum mismatch for $ASSET"
grep -Fq "\"release_version\": \"$VERSION\"" "$ASSET_DIR/release-manifest.json" || die "release-manifest.json is not for $VERSION"

if [ -e "$LINK" ] || [ -L "$LINK" ]; then
  [ -L "$LINK" ] || die "$LINK already exists; choose another name"
  case "$(readlink "$LINK")" in "$STORE"/*) ;; *) die "$LINK is not managed by this installer" ;; esac
fi

mkdir -p "$PREFIX/bin" "$STORE/versions"
printf '%s\n' 'managed by grok-lhc install.sh' > "$STORE/.grok-lhc-managed"
DEST="$STORE/versions/$VERSION"
STAGE="$STORE/versions/.${VERSION}.tmp.$$"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin"
install -m 0755 "$ASSET_DIR/$ASSET" "$STAGE/bin/grok"
install -m 0644 "$ASSET_DIR/release-manifest.json" "$STAGE/release-manifest.json"
rm -rf "$DEST"
mv "$STAGE" "$DEST"
ln -sfn "$DEST" "$STORE/current"
ln -sfn "$STORE/current/bin/grok" "$LINK"
printf '%s\n' "$VERSION" > "$STORE/installed-version"
printf '%s\n' "$NAME" > "$STORE/installed-name"
printf '%s\n' "$PREFIX" > "$STORE/installed-prefix"
printf 'Installed Grok-LHC %s (%s) as %s\n' "$VERSION" "$PLATFORM" "$LINK"
printf 'Full transcripts are retained separately under GROK_LHC_ROOT (default: ~/.grok-lhc).\n'
