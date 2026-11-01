#!/bin/sh
# Install the `oracle` and `ox` binaries from the latest GitHub release.
#   curl -fsSL https://github.com/dot-protocol/dot-oracle/releases/latest/download/install.sh -o install.sh && sh install.sh
set -eu
command -v curl >/dev/null || { echo "curl is not installed; install it first (Debian/Ubuntu: apt-get install -y curl ca-certificates)" >&2; exit 1; }
# A stalled connection fails in seconds with a reason instead of hanging (75-135 s to a 404 was measured on 1 Nov 2026).
fetch() { curl -fsSL --connect-timeout 20 --retry 2 "$@"; }
REPO="${ORACLE_REPO:-dot-protocol/dot-oracle}"
# ORACLE_DOWNLOAD_BASE points it at another copy of the release files (the release workflow tests from one).
BASE="${ORACLE_DOWNLOAD_BASE:-https://github.com/$REPO/releases/latest/download}"
DEST="${ORACLE_BIN_DIR:-$HOME/.local/bin}"
os="$(uname -s | tr '[:upper:]' '[:lower:]')"; arch="$(uname -m)"
case "$arch" in arm64|aarch64) arch=aarch64 ;; x86_64|amd64) arch=x86_64 ;; *) echo "unsupported arch: $arch" >&2; exit 1 ;; esac
case "$os" in darwin) target="$arch-apple-darwin" ;; linux) target="$arch-unknown-linux-musl" ;; *) echo "unsupported OS: $os" >&2; exit 1 ;; esac
url="${BASE%/}/oracle-$target.tar.gz"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
echo "downloading $url"
if ! fetch "$url" -o "$tmp/oracle.tar.gz"; then
  echo "download failed: $url; nothing installed" >&2
  exit 1
fi
if ! fetch "$url.sha256" -o "$tmp/sum"; then
  echo "checksum download failed; nothing installed" >&2
  exit 1
fi
want="$(awk 'NR == 1 {print $1}' "$tmp/sum" | tr '[:upper:]' '[:lower:]')"
printf '%s\n' "$want" | grep -E '^[0-9a-f]{64}$' >/dev/null || { echo "invalid checksum; nothing installed" >&2; exit 1; }
have="$( (sha256sum "$tmp/oracle.tar.gz" 2>/dev/null || shasum -a 256 "$tmp/oracle.tar.gz") | cut -d' ' -f1)"
[ "$want" = "$have" ] || { echo "checksum mismatch; nothing installed" >&2; exit 1; }
mkdir -p "$DEST"; tar -xzf "$tmp/oracle.tar.gz" -C "$tmp"
install -m 755 "$tmp/oracle" "$tmp/ox" "$DEST/"
echo "installed oracle and ox to $DEST"
case ":$PATH:" in *":$DEST:"*) ;; *) echo "add to PATH:  export PATH=\"$DEST:\$PATH\"" ;; esac
