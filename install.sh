#!/bin/sh
# Install the latest hasp release (the `hasp` CLI and the `hasp-desktop` app):
#
#   curl -fsSL https://raw.githubusercontent.com/bnahor/hasp/main/install.sh | sh
#
# HASP_REPO picks another repository (owner/name), HASP_BIN_DIR another
# destination (default ~/.local/bin). From a checkout instead:
# `cargo install --locked --path crates/hasp-cli`.
set -eu

REPO="${HASP_REPO:-bnahor/hasp}"

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Darwin-arm64)  target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64)  target=x86_64-unknown-linux-gnu ;;
  *) echo "install.sh: unsupported platform $os-$arch" >&2; exit 1 ;;
esac

name="hasp-$target"
url="https://github.com/$REPO/releases/latest/download/$name.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "downloading $url"
if ! curl -fsSL "$url" -o "$tmp/$name.tar.gz"; then
  echo "install.sh: no published release found for $target" >&2
  exit 1
fi
if curl -fsSL "$url.sha256.txt" -o "$tmp/sum.txt" 2>/dev/null; then
  (cd "$tmp" && shasum -a 256 -c sum.txt >/dev/null) || { echo "checksum mismatch" >&2; exit 1; }
fi

tar -C "$tmp" -xzf "$tmp/$name.tar.gz"
dest="${HASP_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$dest"
install -m 0755 "$tmp/$name/hasp" "$dest/hasp"
echo "installed $dest/hasp"
if [ -f "$tmp/$name/hasp-desktop" ]; then
  install -m 0755 "$tmp/$name/hasp-desktop" "$dest/hasp-desktop"
  echo "installed $dest/hasp-desktop"
fi
case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "note: add $dest to PATH" ;;
esac
"$dest/hasp" --version || true
