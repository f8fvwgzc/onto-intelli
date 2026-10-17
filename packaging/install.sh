#!/bin/sh
# Install the `onto` binary from GitHub releases (falls back to building from source).
#
#   curl -fsSL https://raw.githubusercontent.com/f8fvwgzc/onto-intelli/main/packaging/install.sh | sh
#
# Environment: ONTO_VERSION (default: latest), ONTO_INSTALL_DIR (default: ~/.local/bin),
#              ONTO_REPO (default: f8fvwgzc/onto-intelli)
set -eu

REPO="${ONTO_REPO:-f8fvwgzc/onto-intelli}"
VERSION="${ONTO_VERSION:-latest}"
DIR="${ONTO_INSTALL_DIR:-$HOME/.local/bin}"

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64)              target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64)              target=aarch64-apple-darwin ;;
  Darwin-x86_64)             target=x86_64-apple-darwin ;;
  *) target="" ;;
esac

if [ "$VERSION" = latest ]; then
  url="https://github.com/$REPO/releases/latest/download/onto-$target.tar.gz"
else
  url="https://github.com/$REPO/releases/download/$VERSION/onto-$target.tar.gz"
fi

mkdir -p "$DIR"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ -n "$target" ] && curl -fsSL "$url" -o "$tmp/onto.tar.gz" 2>/dev/null; then
  if curl -fsSL "$url.sha256" -o "$tmp/onto.tar.gz.sha256" 2>/dev/null; then
    expected=$(cut -d' ' -f1 "$tmp/onto.tar.gz.sha256")
    actual=$( (sha256sum "$tmp/onto.tar.gz" 2>/dev/null || shasum -a 256 "$tmp/onto.tar.gz") | cut -d' ' -f1)
    [ "$expected" = "$actual" ] || { echo "checksum mismatch" >&2; exit 1; }
  fi
  tar -xzf "$tmp/onto.tar.gz" -C "$tmp"
  install -m 0755 "$(find "$tmp" -type f -name onto | head -n 1)" "$DIR/onto"
  echo "Installed onto ($target) to $DIR/onto"
elif command -v cargo >/dev/null 2>&1; then
  echo "No prebuilt binary for $os-$arch; building from source with cargo..."
  cargo install --locked --git "https://github.com/$REPO" onto-intelli-cli --root "$tmp/root"
  install -m 0755 "$tmp/root/bin/onto" "$DIR/onto"
  echo "Installed onto to $DIR/onto"
else
  echo "No prebuilt binary for $os-$arch and cargo is not installed (https://rustup.rs)." >&2
  exit 1
fi

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "Add $DIR to your PATH, e.g.: export PATH=\"$DIR:\$PATH\"" ;;
esac
"$DIR/onto" --version
