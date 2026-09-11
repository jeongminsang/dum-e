#!/bin/sh
set -e

# DUM-E Coding Agent Installer (standalone binary, Bun is NOT required)
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/jeongminsang/dum-e/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/jeongminsang/dum-e/main/scripts/install.sh | sh -s -- --ref v0.1.0
#   sh scripts/install.sh --dev

REPO="jeongminsang/dum-e"
INSTALL_DIR="${DUME_INSTALL_DIR:-$HOME/.local/bin}"
GITHUB_API="${DUME_GITHUB_API:-https://api.github.com}"
GITHUB_RELEASES="${DUME_GITHUB_RELEASES:-https://github.com/${REPO}/releases/download}"

REF=""
DEV_MODE=0

usage() {
    cat <<'EOF'
DUM-E Installer — standalone binary (Bun/Node is not required)

Usage:
  curl -fsSL https://raw.githubusercontent.com/jeongminsang/dum-e/main/scripts/install.sh | sh
  sh install.sh [--ref <tag>]
  sh scripts/install.sh --dev

Options:
  --ref <tag>, -r <tag>   Exact GitHub release tag (default: latest)
  --dev                   Build and install from current local checkout via Bun
  -h, --help              Show this help

Environment:
  DUME_INSTALL_DIR        Install directory (default: ~/.local/bin)
  GITHUB_TOKEN            Optional GitHub token for API rate limits
EOF
}

die() {
    echo "error: $*" >&2
    exit 1
}

detect_target() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)  PLATFORM="linux" ;;
        Darwin) PLATFORM="darwin" ;;
        *)      die "Unsupported OS: $OS. Supported: Linux, macOS" ;;
    esac

    case "$ARCH" in
        x86_64|amd64)  ARCH="x64" ;;
        arm64|aarch64) ARCH="arm64" ;;
        *)             die "Unsupported architecture: $ARCH. Supported: x64, arm64" ;;
    esac

    TARGET="${PLATFORM}-${ARCH}"
    BINARY_NAME="dume-${TARGET}"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --ref|-r)
            [ "$#" -ge 2 ] || die "--ref requires a tag"
            REF="$2"
            shift 2
            ;;
        --dev)
            DEV_MODE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "Unknown option: $1"
            ;;
    esac
done

if [ "$DEV_MODE" -eq 1 ]; then
    echo "==> Building DUM-E standalone binary from local checkout..."
    command -v bun >/dev/null 2>&1 || die "Bun is required to build from source (--dev)."
    mkdir -p "$INSTALL_DIR"
    bun build --compile ./packages/coding-agent/src/cli.ts --outfile "$INSTALL_DIR/dume"
    chmod +x "$INSTALL_DIR/dume"
    echo "==> Successfully installed dume to $INSTALL_DIR/dume"
    exit 0
fi

detect_target

echo "==> Installing DUM-E for ${TARGET} into ${INSTALL_DIR}..."

if [ -z "$REF" ]; then
    AUTH_HEADER=""
    if [ -n "$GITHUB_TOKEN" ]; then
        AUTH_HEADER="Authorization: Bearer $GITHUB_TOKEN"
    fi
    TAG_URL="${GITHUB_API}/repos/${REPO}/releases/latest"
    if [ -n "$AUTH_HEADER" ]; then
        REF=$(curl -fsSL -H "$AUTH_HEADER" "$TAG_URL" 2>/dev/null | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' || true)
    else
        REF=$(curl -fsSL "$TAG_URL" 2>/dev/null | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' || true)
    fi
    if [ -z "$REF" ]; then
        REF="v0.1.0"
    fi
fi

DOWNLOAD_URL="${GITHUB_RELEASES}/${REF}/${BINARY_NAME}"
TMP_DEST="/tmp/dume-install-$$-${BINARY_NAME}"

echo "==> Downloading ${DOWNLOAD_URL}..."
if ! curl -fsSL -o "$TMP_DEST" "$DOWNLOAD_URL"; then
    die "Failed to download DUM-E binary from ${DOWNLOAD_URL}. Release asset may not be published yet."
fi

mkdir -p "$INSTALL_DIR"
mv "$TMP_DEST" "$INSTALL_DIR/dume"
chmod +x "$INSTALL_DIR/dume"

echo "==> DUM-E successfully installed to ${INSTALL_DIR}/dume!"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo ""
        echo "Note: ${INSTALL_DIR} is not in your PATH."
        echo "Add it to your shell config (~/.zshrc or ~/.bashrc):"
        echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
        echo ""
        ;;
esac

echo "Run 'dume doctor' or 'dume --help' to get started."
