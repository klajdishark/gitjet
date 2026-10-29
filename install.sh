#!/bin/sh
# Install the gitjet binary on macOS or Linux.
#   curl -fsSL https://github.com/kashari/gitjet/releases/latest/download/install.sh | sh
# Options (environment variables):
#   GITJET_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   GITJET_VERSION      tag to install, e.g. v0.1.0 (default: latest)
set -eu

REPO="${GITJET_REPO:-kashari/gitjet}"
INSTALL_DIR="${GITJET_INSTALL_DIR:-$HOME/.local/bin}"

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Darwin) os_part="apple-darwin" ;;
  Linux) os_part="unknown-linux-musl" ;;
  *) echo "gitjet: unsupported OS '$os' (on Windows use install.ps1)" >&2; exit 1 ;;
esac
case "$arch" in
  x86_64 | amd64) arch_part="x86_64" ;;
  arm64 | aarch64) arch_part="aarch64" ;;
  *) echo "gitjet: unsupported architecture '$arch'" >&2; exit 1 ;;
esac
target="${arch_part}-${os_part}"

if [ -n "${GITJET_VERSION:-}" ]; then
  base="https://github.com/${REPO}/releases/download/${GITJET_VERSION}"
else
  base="https://github.com/${REPO}/releases/latest/download"
fi
url="${base}/gitjet-${target}.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading ${url}"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$url" -o "$tmp/gitjet.tar.gz"
  curl -fsSL "$url.sha256" -o "$tmp/gitjet.tar.gz.sha256" || true
else
  wget -qO "$tmp/gitjet.tar.gz" "$url"
  wget -qO "$tmp/gitjet.tar.gz.sha256" "$url.sha256" || true
fi

if [ -s "$tmp/gitjet.tar.gz.sha256" ]; then
  expected=$(cut -d ' ' -f 1 < "$tmp/gitjet.tar.gz.sha256")
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/gitjet.tar.gz" | cut -d ' ' -f 1)
  else
    actual=$(shasum -a 256 "$tmp/gitjet.tar.gz" | cut -d ' ' -f 1)
  fi
  if [ "$expected" != "$actual" ]; then
    echo "gitjet: checksum mismatch, aborting" >&2
    exit 1
  fi
fi

tar -xzf "$tmp/gitjet.tar.gz" -C "$tmp"
mkdir -p "$INSTALL_DIR"
mv "$tmp/gitjet" "$INSTALL_DIR/gitjet"
chmod +x "$INSTALL_DIR/gitjet"
echo "Installed gitjet to $INSTALL_DIR/gitjet"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Note: $INSTALL_DIR is not on your PATH. Add this to your shell profile:"
     echo "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
