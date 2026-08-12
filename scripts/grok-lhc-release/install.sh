#!/bin/sh
set -eu

PREFIX="${GROK_LHC_PREFIX:-${HOME}/.local}"
STORE="${GROK_LHC_INSTALL_ROOT:-${XDG_DATA_HOME:-${HOME}/.local/share}/grok-lhc}"
VERSION="${GROK_LHC_VERSION:-}"
ASSET_DIR="${GROK_LHC_ASSET_DIR:-}"
NAME="${GROK_LHC_NAME:-}"
UNINSTALL=0

die() { printf 'grok-lhc installer: %s\n' "$*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) VERSION=$2; shift 2 ;;
    --name) NAME=$2; shift 2 ;;
    --prefix) PREFIX=$2; shift 2 ;;
    --install-root) STORE=$2; shift 2 ;;
    --asset-dir) ASSET_DIR=$2; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) printf '%s\n' 'Usage: install.sh --version VERSION [--asset-dir DIR] [--name NAME] [--prefix DIR] [--install-root DIR] [--uninstall]'; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

case "$STORE" in ''|/|"$HOME") die "refusing unsafe install root: $STORE" ;; esac

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

[ -n "$NAME" ] || NAME=grok
case "$NAME" in */*) die "--name must be a command name" ;; esac
LINK="$PREFIX/bin/$NAME"
[ -n "$VERSION" ] || die "--version is required"
[ -n "$ASSET_DIR" ] || die "--asset-dir is required for candidate installation"
case "$VERSION" in *[!0-9A-Za-z.+-]*|'') die "invalid version: $VERSION" ;; esac
if [ -e "$STORE" ] && [ ! -f "$STORE/.grok-lhc-managed" ]; then
  die "$STORE already exists and is not managed by this installer"
fi
if [ -f "$STORE/installed-name" ]; then
  installed_name=$(cat "$STORE/installed-name")
  requested_name=${NAME:-grok}
  [ "$requested_name" = "$installed_name" ] || die "managed store is installed as $installed_name; use that name"
fi
ASSET="grok-${VERSION}-linux-x86_64"
[ -f "$ASSET_DIR/$ASSET" ] || die "candidate is missing $ASSET"
[ -f "$ASSET_DIR/SHA256SUMS" ] || die "candidate is missing SHA256SUMS"
[ -f "$ASSET_DIR/release-manifest.json" ] || die "candidate is missing release-manifest.json"
expected=$(awk -v name="$ASSET" '$2 == name { print $1 }' "$ASSET_DIR/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS does not list $ASSET"
actual=$(sha256sum "$ASSET_DIR/$ASSET" | awk '{print $1}')
[ "$actual" = "$expected" ] || die "checksum mismatch for $ASSET"

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
printf 'Installed Grok-LHC v%s at %s\n' "$VERSION" "$LINK"
printf 'Full transcripts are retained separately under GROK_LHC_ROOT (default: ~/.grok-lhc).\n'
