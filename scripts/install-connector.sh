#!/bin/sh
# install-connector.sh — installs the `radix-connector-mcp` binary from GitHub
# Releases (no crates.io / npm involved). Works on Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/genkipool/radixdlt-rust-sdk/main/scripts/install-connector.sh | sh
#
# Optional: pass a release tag as the first argument (default: latest connector
# release), and set BIN_DIR to change the install location (default ~/.local/bin).
set -eu

REPO="genkipool/radixdlt-rust-sdk"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
TAG="${1:-}"

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

# --- Detect the platform target triple -------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64)  target="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
      *) die "unsupported Linux architecture: $arch" ;;
    esac ;;
  Darwin)
    case "$arch" in
      x86_64|amd64)  target="x86_64-apple-darwin" ;;
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      *) die "unsupported macOS architecture: $arch" ;;
    esac ;;
  *) die "unsupported OS: $os (use install-connector.ps1 on Windows)" ;;
esac

# --- Resolve the release tag ------------------------------------------------
if [ -z "$TAG" ]; then
  say "Resolving the latest connector release…"
  TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases" \
    | grep '"tag_name"' | grep 'connector-v' | head -n 1 \
    | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
  [ -n "$TAG" ] || die "could not find a connector release. Pass a tag explicitly, or install with cargo (see the README)."
fi

url="https://github.com/$REPO/releases/download/$TAG/radix-connector-mcp-$target"
dest="$BIN_DIR/radix-connector-mcp"

say "Downloading radix-connector-mcp ($TAG, $target)…"
mkdir -p "$BIN_DIR"
# Download to a temp file, then atomically move it into place. Overwriting the
# binary directly fails with "Text file busy" (curl error 23) when the connector
# is already running — e.g. your MCP client has it open. A rename swaps the name
# while any running process keeps the old binary until it exits.
tmp="$dest.new.$$"
curl -fsSL "$url" -o "$tmp" || { rm -f "$tmp"; die "download failed: $url"; }
chmod +x "$tmp"
mv -f "$tmp" "$dest" || { rm -f "$tmp"; die "could not install to $dest"; }

say ""
say "Installed: $dest"
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *) say "NOTE: $BIN_DIR is not on your PATH. Add it, e.g.:"
     say "      echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.profile && . ~/.profile" ;;
esac
say ""
say "Register it with your MCP client, e.g. Claude Code:"
say "  claude mcp add radix-connector -- $dest"
say ""
say "Or in a JSON MCP config:"
say "  { \"mcpServers\": { \"radix-connector\": { \"command\": \"$dest\" } } }"
