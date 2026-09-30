#!/usr/bin/env bash
# Installs the latest bracket release for this machine:
#
#   curl -fsSL https://raw.githubusercontent.com/xav-db/bracket-cli/main/install/install.sh | bash
#
# Set BRACKET_INSTALL_DIR to install somewhere other than ~/.local/bin.
set -euo pipefail

REPO="xav-db/bracket-cli"
INSTALL_DIR="${BRACKET_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) echo "bracket: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=amd64 ;;
  arm64 | aarch64) arch=arm64 ;;
  *) echo "bracket: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

url="https://github.com/$REPO/releases/latest/download/bracket-cli-$os-$arch"
mkdir -p "$INSTALL_DIR"
tmp="$(mktemp "$INSTALL_DIR/.bracket.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

echo "Downloading $url"
curl -fsSL "$url" -o "$tmp"
chmod 755 "$tmp"
# Rename rather than write in place: overwriting a running binary fails on Linux
# ("text file busy") and can get it killed on macOS. This is what `bracket update` relies on.
mv -f "$tmp" "$INSTALL_DIR/bracket"
trap - EXIT

echo "Installed $("$INSTALL_DIR/bracket" --version) to $INSTALL_DIR"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    case "${SHELL:-}" in
      *zsh) rc="$HOME/.zshrc" ;;
      *bash) rc="$HOME/.bashrc" ;;
      *) rc="" ;;
    esac
    line="export PATH=\"$INSTALL_DIR:\$PATH\""
    if [[ -n "$rc" ]]; then
      grep -qxF "$line" "$rc" 2>/dev/null || printf '\n%s\n' "$line" >> "$rc"
      echo "Added $INSTALL_DIR to your PATH in $rc. Open a new terminal to use bracket."
    else
      echo "Add $INSTALL_DIR to your PATH to use bracket."
    fi
    ;;
esac
