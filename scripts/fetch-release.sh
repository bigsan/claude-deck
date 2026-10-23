#!/bin/sh
# The [[build]] step of herdr-plugin.toml: download this version's release binary
# for this machine into bin/claude-deck and check it against its .sha256. When there
# is no release to download (an unsupported platform, no network, a private repo)
# but claude-deck is already installed, bin/claude-deck.sh falls back to that one.
# Needs curl, tar and shasum or sha256sum.

set -eu
cd "$(dirname "$0")/.."

repo=bigsan/claude-deck
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' herdr-plugin.toml | head -n 1)
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Linux-x86_64) target=x86_64-unknown-linux-musl ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
    *) target= ;;
esac

installed() {
    for bin in "$(command -v claude-deck 2>/dev/null)" /opt/homebrew/bin/claude-deck /usr/local/bin/claude-deck \
        /home/linuxbrew/.linuxbrew/bin/claude-deck "$HOME/.cargo/bin/claude-deck"; do
        if [ -n "$bin" ] && [ -x "$bin" ]; then
            echo "$bin"
            return 0
        fi
    done
    return 1
}

fallback() {
    if bin=$(installed); then
        echo "claude-deck: $1; the plugin will run $bin"
        exit 0
    fi
    echo "claude-deck: $1, and claude-deck is not installed." >&2
    echo "Install it (https://github.com/bigsan/claude-deck#install), then install the plugin again." >&2
    exit 1
}

[ -n "$target" ] || fallback "no release build for $(uname -s) $(uname -m)"
[ -n "$version" ] || fallback "no version in herdr-plugin.toml"

name="claude-deck-v$version-$target.tar.gz"
url="https://github.com/$repo/releases/download/v$version/$name"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL -o "$tmp/$name" "$url" && curl -fsSL -o "$tmp/$name.sha256" "$url.sha256" \
    || fallback "could not download $url"
if command -v shasum >/dev/null 2>&1; then
    (cd "$tmp" && shasum -a 256 -c "$name.sha256" >/dev/null) || { echo "claude-deck: $name does not match its checksum" >&2; exit 1; }
else
    (cd "$tmp" && sha256sum -c "$name.sha256" >/dev/null) || { echo "claude-deck: $name does not match its checksum" >&2; exit 1; }
fi
tar -xzf "$tmp/$name" -C "$tmp"
mkdir -p bin
mv "$tmp/claude-deck-v$version-$target/claude-deck" bin/claude-deck
chmod +x bin/claude-deck
if ! bin/claude-deck --version >/dev/null 2>&1; then
    rm -f bin/claude-deck
    fallback "the release binary $name does not run here"
fi
echo "claude-deck: installed $name"
