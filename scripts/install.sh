#!/bin/sh
set -eu

REPO="jeongminsang/dum-e"
INSTALL_DIR="${DUME_INSTALL_DIR:-$HOME/.local/bin}"
GITHUB_API="${DUME_GITHUB_API:-https://api.github.com}"
GITHUB_RELEASES="${DUME_GITHUB_RELEASES:-https://github.com/${REPO}/releases/download}"
REF=""
DEV_MODE=0
TEMP_DIR=""
INSTALL_TEMP=""

usage() {
    printf '%s\n' \
        'DUM-E native Rust installer (no Node or Bun runtime)' \
        'Usage: sh scripts/install.sh [--ref vVERSION | --dev]' \
        '  --ref TAG   Install a checksummed GitHub release (default: latest)' \
        '  --dev       Build and install the local Rust workspace using Cargo' \
        '  DUME_INSTALL_DIR overrides ~/.local/bin'
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "$INSTALL_TEMP" ]; then rm -f "$INSTALL_TEMP"; fi
    if [ -n "$TEMP_DIR" ]; then rm -rf "$TEMP_DIR"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

while [ "$#" -gt 0 ]; do
    case "$1" in
        --ref|-r)
            [ "$#" -ge 2 ] || die '--ref requires a release tag'
            REF="$2"
            shift 2
            ;;
        --dev) DEV_MODE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "Unknown option: $1" ;;
    esac
done
[ "$DEV_MODE" -eq 0 ] || [ -z "$REF" ] || die '--dev and --ref are mutually exclusive'
TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dume-install.XXXXXX")"

if [ "$DEV_MODE" -eq 1 ]; then
    command -v cargo >/dev/null || die 'Cargo is required for --dev'
    PROJECT_ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
    [ -f "$PROJECT_ROOT/Cargo.lock" ] || die 'The workspace Cargo.lock is missing'
    cargo install --locked --path "$PROJECT_ROOT/crates/dume-cli" \
        --root "$TEMP_DIR/build" --target-dir "${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
    BINARY="$TEMP_DIR/build/bin/dume"
else
    command -v curl >/dev/null || die 'curl is required'
    command -v tar >/dev/null || die 'tar is required'
    case "$(uname -s)" in
        Darwin) PLATFORM=darwin ;;
        Linux) PLATFORM=linux ;;
        *) die 'Use the native release ZIP on Windows; this installer supports macOS and Linux' ;;
    esac
    case "$(uname -m)" in
        arm64|aarch64) ARCH=arm64 ;;
        x86_64|amd64) ARCH=x64 ;;
        *) die 'Unsupported architecture (expected arm64 or x64)' ;;
    esac
    ARCHIVE="dume-${PLATFORM}-${ARCH}.tar.gz"
    if [ -z "$REF" ]; then
        TAG_URL="${GITHUB_API}/repos/${REPO}/releases/latest"
        AUTH_TOKEN="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
        if [ -z "$AUTH_TOKEN" ] && command -v gh >/dev/null 2>&1; then
            AUTH_TOKEN="$(gh auth token 2>/dev/null || true)"
        fi
        if [ -n "$AUTH_TOKEN" ]; then
            curl -fsSL --connect-timeout 15 --max-time 120 \
                -H "Authorization: Bearer $AUTH_TOKEN" "$TAG_URL" -o "$TEMP_DIR/latest.json"
        else
            curl -fsSL --connect-timeout 15 --max-time 120 "$TAG_URL" -o "$TEMP_DIR/latest.json"
        fi
        REF="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$TEMP_DIR/latest.json")"
    fi
    printf '%s\n' "$REF" | LC_ALL=C grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?(\+[A-Za-z0-9.-]+)?$' \
        || die 'Could not resolve a valid release tag; no fallback release is installed'
    printf 'Downloading %s (%s)\n' "$ARCHIVE" "$REF"
    curl -fsSL --connect-timeout 15 --max-time 120 \
        "${GITHUB_RELEASES}/${REF}/${ARCHIVE}" -o "$TEMP_DIR/$ARCHIVE"
    curl -fsSL --connect-timeout 15 --max-time 120 \
        "${GITHUB_RELEASES}/${REF}/SHA256SUMS" -o "$TEMP_DIR/SHA256SUMS"
    EXPECTED="$(awk -v asset="$ARCHIVE" '$2 == asset { print $1 }' "$TEMP_DIR/SHA256SUMS")"
    printf '%s\n' "$EXPECTED" | LC_ALL=C grep -Eq '^[0-9a-fA-F]{64}$' \
        || die 'Release checksum missing or malformed'
    [ "$(printf '%s\n' "$EXPECTED" | wc -l | tr -d ' ')" -eq 1 ] \
        || die 'Duplicate release checksums'
    if command -v sha256sum >/dev/null; then
        ACTUAL="$(sha256sum "$TEMP_DIR/$ARCHIVE" | awk '{ print $1 }')"
    elif command -v shasum >/dev/null; then
        ACTUAL="$(shasum -a 256 "$TEMP_DIR/$ARCHIVE" | awk '{ print $1 }')"
    else
        die 'sha256sum or shasum is required'
    fi
    EXPECTED="$(printf '%s' "$EXPECTED" | tr 'A-F' 'a-f')"
    [ "$ACTUAL" = "$EXPECTED" ] || die 'Release checksum mismatch; installed executable is unchanged'
    tar -xzf "$TEMP_DIR/$ARCHIVE" -C "$TEMP_DIR" dume/dume
    BINARY="$TEMP_DIR/dume/dume"
fi

[ -f "$BINARY" ] && [ ! -L "$BINARY" ] || die 'Archive does not contain a regular dume executable'
chmod 755 "$BINARY"
ACTUAL_VERSION="$("$BINARY" --version)"
if [ "$DEV_MODE" -eq 0 ]; then
    [ "$ACTUAL_VERSION" = "dume ${REF#v}" ] || die 'Release binary version mismatch'
fi
printf '%s\n' "$ACTUAL_VERSION"
mkdir -p "$INSTALL_DIR"
INSTALL_TEMP="$(mktemp "$INSTALL_DIR/.dume-install.XXXXXX")"
cp "$BINARY" "$INSTALL_TEMP"
chmod 755 "$INSTALL_TEMP"
mv -f "$INSTALL_TEMP" "$INSTALL_DIR/dume"
INSTALL_TEMP=""
printf 'Installed %s/dume\n' "$INSTALL_DIR"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) printf 'Add %s to PATH.\n' "$INSTALL_DIR" ;;
esac
printf '%s\n' 'Run dume --help or dume login to get started.'
