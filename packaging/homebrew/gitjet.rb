# Homebrew formula for a tap repository (klajdishark/homebrew-tap → Formula/gitjet.rb).
# Install with: brew install klajdishark/tap/gitjet
# Update version and the sha256 values (from the .sha256 files of the GitHub release) for each release.
class Gitjet < Formula
  desc "Keyboard-driven terminal Git client with IDE-grade features"
  homepage "https://github.com/klajdishark/gitjet"
  version "0.2.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/klajdishark/gitjet/releases/download/v#{version}/gitjet-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    else
      url "https://github.com/klajdishark/gitjet/releases/download/v#{version}/gitjet-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/klajdishark/gitjet/releases/download/v#{version}/gitjet-aarch64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    else
      url "https://github.com/klajdishark/gitjet/releases/download/v#{version}/gitjet-x86_64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    end
  end

  depends_on "git"

  def install
    bin.install "gitjet"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/gitjet --version")
  end
end
