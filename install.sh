#!/bin/sh
# Install a published harness release.
#
# Requires a release to exist: set HARNESS_REPO to the GitHub repository that
# publishes them (owner/name). Without a release, build from a checkout with
# `cargo install --path crates/harness-cli` — that path needs no network.
set -eu

REPO="${HARNESS_REPO:-}"
if [ -z "$REPO" ]; then
  echo "install.sh: set HARNESS_REPO=owner/name (no release channel is published yet)" >&2
  echo "            or build from a checkout: cargo install --path crates/harness-cli" >&2
  exit 1
fi

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Darwin-arm64)  target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64)  target=x86_64-unknown-linux-gnu ;;
  *) echo "install.sh: unsupported platform $os-$arch" >&2; exit 1 ;;
esac

name="harness-$target"
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
dest="${HARNESS_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$dest"
install -m 0755 "$tmp/$name/harness" "$dest/harness"
echo "installed $dest/harness"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "note: add $dest to PATH" ;;
esac
"$dest/harness" --version || true
