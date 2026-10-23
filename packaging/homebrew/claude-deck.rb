# Template for Formula/claude-deck.rb in the bigsan/homebrew-tap repository.
# scripts/brew-formula.sh <version> prints it with the version and checksums of that
# release filled in.
class ClaudeDeck < Formula
  desc "Watch Claude Code sessions: scratchpad files, tasks and every tool call"
  homepage "https://github.com/bigsan/claude-deck"
  version "@VERSION@"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigsan/claude-deck/releases/download/v@VERSION@/claude-deck-v@VERSION@-aarch64-apple-darwin.tar.gz"
      sha256 "@SHA256_aarch64-apple-darwin@"
    end
    on_intel do
      url "https://github.com/bigsan/claude-deck/releases/download/v@VERSION@/claude-deck-v@VERSION@-x86_64-apple-darwin.tar.gz"
      sha256 "@SHA256_x86_64-apple-darwin@"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/bigsan/claude-deck/releases/download/v@VERSION@/claude-deck-v@VERSION@-aarch64-unknown-linux-musl.tar.gz"
      sha256 "@SHA256_aarch64-unknown-linux-musl@"
    end
    on_intel do
      url "https://github.com/bigsan/claude-deck/releases/download/v@VERSION@/claude-deck-v@VERSION@-x86_64-unknown-linux-musl.tar.gz"
      sha256 "@SHA256_x86_64-unknown-linux-musl@"
    end
  end

  def install
    bin.install "claude-deck"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/claude-deck --version")
  end
end
