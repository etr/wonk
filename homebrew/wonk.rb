# typed: false
# frozen_string_literal: true

# Homebrew formula for wonk - fast code structure indexer.
#
# To use this formula in a tap, create a repository named "homebrew-wonk"
# with this file at Formula/wonk.rb. Then users can install via:
#
#   brew tap etr/wonk
#   brew install wonk
#
# To update the formula for a new release:
#   1. Replace VERSION with the new version string (without "v" prefix)
#   2. Update the sha256 checksums for each platform binary
#      (run: shasum -a 256 wonk-VERSION-TARGET)
#
class Wonk < Formula
  desc "Fast code structure indexer with tree-sitter parsing"
  homepage "https://github.com/etr/wonk"
  version "VERSION"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/etr/wonk/releases/download/v#{version}/wonk-#{version}-aarch64-apple-darwin"
      sha256 "SHA256_MACOS_ARM64"
    else
      url "https://github.com/etr/wonk/releases/download/v#{version}/wonk-#{version}-x86_64-apple-darwin"
      sha256 "SHA256_MACOS_X86_64"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/etr/wonk/releases/download/v#{version}/wonk-#{version}-aarch64-unknown-linux-musl"
      sha256 "SHA256_LINUX_ARM64"
    else
      url "https://github.com/etr/wonk/releases/download/v#{version}/wonk-#{version}-x86_64-unknown-linux-musl"
      sha256 "SHA256_LINUX_X86_64"
    end
  end

  def install
    binary = Dir["wonk-*"].first || "wonk"
    bin.install binary => "wonk"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/wonk --version")
  end
end
