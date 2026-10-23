#!/bin/sh
# Prints the Homebrew formula for a published release: packaging/homebrew/claude-deck.rb
# with the version and each target's sha256 (from the release's .sha256 files) filled in.
#   scripts/brew-formula.sh 0.2.0 > ../homebrew-tap/Formula/claude-deck.rb
# Needs curl, and the repository to be public (release assets of a private one need a token).

set -eu
cd "$(dirname "$0")/.."
version=${1:?usage: scripts/brew-formula.sh <version>}
base="https://github.com/bigsan/claude-deck/releases/download/v$version"
formula=$(sed "s/@VERSION@/$version/g" packaging/homebrew/claude-deck.rb)
for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    sum=$(curl -fsSL "$base/claude-deck-v$version-$target.tar.gz.sha256" | cut -d ' ' -f 1)
    [ -n "$sum" ] || { echo "no checksum for $target" >&2; exit 1; }
    formula=$(printf '%s\n' "$formula" | sed "s/@SHA256_$target@/$sum/")
done
printf '%s\n' "$formula" | sed '1,3d'
