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
  version "0.3.18"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.18/rpi-v0.3.18-aarch64-apple-darwin.tar.gz"
      sha256 "3b0c6d3d6459d46181186e88adf08a5b39c223161cbb1a04581c19cf851c3c9b"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.18/rpi-v0.3.18-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "c38c0c7d8608c3857fce8e6045371943704c0c0f0b689246b4b25890bf1d49c3"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.18/rpi-v0.3.18-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "d96240924432ec3486a8804a3c7b82acbaea872239a5e4ed665ecd5be815de1b"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
