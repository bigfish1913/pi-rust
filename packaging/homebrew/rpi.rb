# Homebrew formula for rpi.
#
# Not in homebrew-core: that requires notability (stars/forks) rpi does not have
# yet. It works today from a tap — publish this file as `Formula/rpi.rb` in a
# repo named `homebrew-tap` under the same owner, then:
#
#   brew tap bigfish1913/tap
#   brew install rpi
#
# Hashes below are for v0.3.2. See packaging/README.md for the refresh procedure.
class Rpi < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://rpi.laofu.online/"
  version "0.3.16"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.16/rpi-v0.3.16-aarch64-apple-darwin.tar.gz"
      sha256 "62b02ebb6cbb158ef0bba0fd27430030ed23759c9affabb846abec33a5d39b42"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.16/rpi-v0.3.16-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "b4cce9da5db9f72c41870e4cce85a6a54c21b1cb4b4f0bc8d55da6b2c441a5c3"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.16/rpi-v0.3.16-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "c7b82a4e8f3b001bce03909cce573dfd834b667b0d885ea3d65ba63588fce082"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
