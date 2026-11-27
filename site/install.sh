#!/bin/sh
# Installs the latest trade-bot release from GitHub (Linux, macOS).
#
#   curl -fsSL https://sc0vu.github.io/trade-bot/install.sh | sh
#
# Detects the platform, downloads the archive and SHA256SUMS from the
# latest release, refuses to install unless the checksum matches, then
# puts the binary in $INSTALL_DIR (default ~/.local/bin) and the example
# configs and scripts in $SHARE_DIR (default ~/.local/share/trade-bot).
# Set VERSION=0.3.0 to pin a release. Needs curl, tar and sha256sum or
# shasum. Windows: download the .zip from the releases page.
set -eu

REPO="sc0Vu/trade-bot"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
SHARE_DIR="${SHARE_DIR:-$HOME/.local/share/trade-bot}"

fail() { echo "install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *) fail "unsupported OS $(uname -s); download a release from https://github.com/$REPO/releases" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=amd64 ;;
  arm64 | aarch64) arch=arm64 ;;
  *) fail "unsupported CPU $(uname -m)" ;;
esac

if [ -z "${VERSION:-}" ]; then
  tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$tag" ] || fail "could not find the latest release"
  VERSION="${tag#v}"
fi
name="trade_bot_${os}_${arch}.${VERSION}"
base="https://github.com/$REPO/releases/download/v${VERSION}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading $name.tar.gz (v$VERSION)"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz" || fail "no $name.tar.gz in v$VERSION"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "no SHA256SUMS in v$VERSION"

want=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d ' ' -f 1)
[ -n "$want" ] || fail "$name.tar.gz is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$name.tar.gz" | cut -d ' ' -f 1)
else
  got=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d ' ' -f 1)
fi
[ "$got" = "$want" ] || fail "checksum mismatch for $name.tar.gz: refusing to install"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$INSTALL_DIR" "$SHARE_DIR"
cp "$tmp/$name/trade-bot" "$INSTALL_DIR/trade-bot"
chmod 755 "$INSTALL_DIR/trade-bot"
cp -R "$tmp/$name/config" "$tmp/$name/strategies" "$SHARE_DIR/"
# macOS marks downloads as quarantined; the checksum was verified above.
if [ "$os" = darwin ]; then
  xattr -d com.apple.quarantine "$INSTALL_DIR/trade-bot" 2>/dev/null || true
fi

echo "installed $("$INSTALL_DIR/trade-bot" --version) to $INSTALL_DIR/trade-bot"
echo "example configs and scripts: $SHARE_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "add it to your PATH:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
