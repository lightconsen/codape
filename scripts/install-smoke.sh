#!/usr/bin/env bash
# Smoke-test scripts/install.sh without GitHub or a release.
#
# Builds (or reuses) a local codape binary, serves a fake release tree over
# loopback HTTP, runs the real installer into a temporary prefix, and checks
# the installed binary and its PATH/uninstall guidance.
#
# Usage: scripts/install-smoke.sh
#   CODAPE_INSTALL_SMOKE_PROFILE=release  build the release profile (default debug)

set -euo pipefail

cd "$(dirname "$0")/.."

profile="${CODAPE_INSTALL_SMOKE_PROFILE:-debug}"
version="v0.0.0-smoke"

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$os" in
    linux)
        case "$arch" in
            x86_64) target="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
            *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
        esac
        ;;
    darwin)
        case "$arch" in
            x86_64) target="x86_64-apple-darwin" ;;
            aarch64|arm64) target="aarch64-apple-darwin" ;;
            *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
        esac
        ;;
    *)
        echo "unsupported OS: $os" >&2
        exit 1
        ;;
esac

if [[ "$profile" == "release" ]]; then
    cargo build --release --package codape-cli --bin codape
    binary="target/release/codape"
else
    cargo build --package codape-cli --bin codape
    binary="target/debug/codape"
fi

tmp=$(mktemp -d)
server_pid=""
cleanup() {
    if [[ -n "$server_pid" ]]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT

release="$tmp/releases/download/$version"
mkdir -p "$release" "$tmp/bin"
cp "$binary" "$release/codape-$target"
tar czf "$release/codape-$target.tar.gz" -C "$release" "codape-$target"
printf '{"tag_name":"%s"}\n' "$version" > "$tmp/releases/latest"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}
digest=$(sha256_of "$release/codape-$target.tar.gz")
printf '%s  %s\n' "$digest" "codape-$target.tar.gz" > "$release/SHA256SUMS"
cp "$release/SHA256SUMS" "$tmp/good-sums"

port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$tmp" >/dev/null 2>&1 &
server_pid=$!
base="http://127.0.0.1:$port"
for _ in $(seq 1 50); do
    curl -fsS "$base/releases/latest" >/dev/null 2>&1 && break
    sleep 0.1
done

echo "== explicit version"
output=$(CODAPE_RELEASE_BASE_URL="$base/releases/download" \
    INSTALL_DIR="$tmp/bin" \
    sh scripts/install.sh "$version")
printf '%s\n' "$output"
"$tmp/bin/codape" --version | grep -qi codape
printf '%s\n' "$output" | grep -q "not on your PATH"
printf '%s\n' "$output" | grep -q "To uninstall"

echo "== latest"
rm -f "$tmp/bin/codape"
CODAPE_RELEASE_BASE_URL="$base/releases/download" \
    CODAPE_API_BASE_URL="$base" \
    INSTALL_DIR="$tmp/bin" \
    sh scripts/install.sh >/dev/null
"$tmp/bin/codape" --version | grep -qi codape

echo "== missing checksums"
rm -f "$tmp/bin/codape" "$release/SHA256SUMS"
if CODAPE_RELEASE_BASE_URL="$base/releases/download" \
    INSTALL_DIR="$tmp/bin" \
    sh scripts/install.sh "$version" >/dev/null 2>&1; then
    echo "install.sh installed a release with no SHA256SUMS" >&2
    exit 1
fi
test ! -e "$tmp/bin/codape"

echo "== checksum mismatch"
printf '%064d  %s\n' 0 "codape-$target.tar.gz" > "$release/SHA256SUMS"
if CODAPE_RELEASE_BASE_URL="$base/releases/download" \
    INSTALL_DIR="$tmp/bin" \
    sh scripts/install.sh "$version" >/dev/null 2>&1; then
    echo "install.sh installed a release with a wrong checksum" >&2
    exit 1
fi
test ! -e "$tmp/bin/codape"
cp "$tmp/good-sums" "$release/SHA256SUMS"

echo "== a release published under the pre-rename names"
# Releases up to v0.1.3 ship `coderex-*` assets whose archives hold a
# `coderex-*` member. The installer must still install them.
legacy="$tmp/legacy/releases/download/$version"
mkdir -p "$legacy"
cp "$binary" "$legacy/coderex-$target"
tar czf "$legacy/coderex-$target.tar.gz" -C "$legacy" "coderex-$target"
printf '%s  %s\n' "$(sha256_of "$legacy/coderex-$target.tar.gz")" \
    "coderex-$target.tar.gz" > "$legacy/SHA256SUMS"
rm -f "$tmp/bin/codape"
CODAPE_RELEASE_BASE_URL="$base/legacy/releases/download" \
    INSTALL_DIR="$tmp/bin" \
    sh scripts/install.sh "$version" >/dev/null
"$tmp/bin/codape" --version | grep -qi codape

echo "== neither name is published"
printf '%064d  %s\n' 0 "something-else.tar.gz" > "$release/SHA256SUMS"
rm -f "$tmp/bin/codape"
if CODAPE_RELEASE_BASE_URL="$base/releases/download" \
    INSTALL_DIR="$tmp/bin" \
    sh scripts/install.sh "$version" >/dev/null 2>&1; then
    echo "install.sh installed a release with no archive for this target" >&2
    exit 1
fi
test ! -e "$tmp/bin/codape"

echo "install smoke: ok"
