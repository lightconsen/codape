#!/bin/sh
# Codape installer — cross-platform shell script.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/lightconsen/codape/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/lightconsen/codape/main/scripts/install.sh | sh -s -- v0.1.0

set -e

REPO="lightconsen/codape"
BINARY="codape"
RELEASE_BASE_URL="${CODAPE_RELEASE_BASE_URL:-https://github.com/$REPO/releases/download}"
API_URL="${CODAPE_API_BASE_URL:-https://api.github.com/repos/$REPO}"

# Determine version
if [ -n "$1" ]; then
    VERSION="$1"
else
    VERSION="latest"
fi

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux)
        case "$ARCH" in
            x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64)
                echo "No prebuilt archive for aarch64 Linux yet; build from source: cargo install --git https://github.com/$REPO.git"
                exit 1 ;;
            *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    darwin)
        case "$ARCH" in
            x86_64) TARGET="x86_64-apple-darwin" ;;
            aarch64|arm64) TARGET="aarch64-apple-darwin" ;;
            *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    *)
        echo "Unsupported OS: $OS"
        exit 1
        ;;
esac

# Resolve latest version from GitHub API
if [ "$VERSION" = "latest" ]; then
    VERSION=$(curl -fsSL "$API_URL/releases/latest" | \
        grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -z "$VERSION" ]; then
        echo "Failed to determine latest version"
        exit 1
    fi
fi

echo "Installing $BINARY $VERSION for $TARGET..."

# Releases up to v0.1.3 were published before the project was renamed and use
# the old prefix for both the asset and the member inside it. Try the current
# name first and fall back, so the script installs either kind of release.
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# SHA-256 of a file, whichever tool this machine has.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        openssl dgst -sha256 -r "$1" | awk '{print $1}'
    fi
}

if ! curl -fsSL "$RELEASE_BASE_URL/$VERSION/SHA256SUMS" -o "$TMPDIR/SHA256SUMS"; then
    echo "Could not download SHA256SUMS for $VERSION; refusing to install an unverified download" >&2
    exit 1
fi

# Pick the first candidate the release actually publishes. A release that
# publishes neither is a hard error, not a silent fallthrough to an unverified
# download.
ARCHIVE=""
for candidate in "${BINARY}-${TARGET}.tar.gz" "coderex-${TARGET}.tar.gz"; do
    if awk -v name="$candidate" '$2 == name { found = 1 } END { exit !found }' "$TMPDIR/SHA256SUMS"; then
        ARCHIVE="$candidate"
        break
    fi
done
if [ -z "$ARCHIVE" ]; then
    echo "SHA256SUMS for $VERSION lists no archive for $TARGET; refusing to install" >&2
    exit 1
fi

# The member inside the archive carries the archive's own prefix, minus the
# extension: `codape-<target>.tar.gz` holds `codape-<target>`.
MEMBER="${ARCHIVE%.tar.gz}"
URL="$RELEASE_BASE_URL/$VERSION/$ARCHIVE"

curl -fsSL "$URL" -o "$TMPDIR/$ARCHIVE"

# The release publishes one SHA256SUMS for every asset. A missing file, a
# missing entry or a mismatch stops the install: an unverified download is a
# download from whatever answered the request.
EXPECTED=$(awk -v name="$ARCHIVE" '$2 == name { print $1; exit }' "$TMPDIR/SHA256SUMS")
if [ -z "$EXPECTED" ]; then
    echo "SHA256SUMS has no entry for $ARCHIVE; refusing to install" >&2
    exit 1
fi
ACTUAL=$(sha256_of "$TMPDIR/$ARCHIVE")
if [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "Checksum mismatch for $ARCHIVE: expected $EXPECTED, got $ACTUAL" >&2
    exit 1
fi

tar xzf "$TMPDIR/$ARCHIVE" -C "$TMPDIR"

# Install to ~/.local/bin or /usr/local/bin
INSTALL_DIR="${INSTALL_DIR:-}"
if [ -z "$INSTALL_DIR" ]; then
    if [ -d "$HOME/.local/bin" ] && echo "$PATH" | grep -q "$HOME/.local/bin"; then
        INSTALL_DIR="$HOME/.local/bin"
    else
        INSTALL_DIR="/usr/local/bin"
    fi
fi

# Create install dir if needed
mkdir -p "$INSTALL_DIR"

# Move binary
cp "$TMPDIR/$MEMBER" "$INSTALL_DIR/$BINARY"
chmod +x "$INSTALL_DIR/$BINARY"

echo "Installed $BINARY to $INSTALL_DIR/$BINARY"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo "Note: $INSTALL_DIR is not on your PATH."
        echo "Add it with: export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
echo "Run '$BINARY --help' to get started"
echo "To uninstall, remove $INSTALL_DIR/$BINARY"
