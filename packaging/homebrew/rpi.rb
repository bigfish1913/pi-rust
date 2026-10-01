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
  version "0.3.9"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.9/rpi-v0.3.9-aarch64-apple-darwin.tar.gz"
      sha256 "57d454531aaaed142a664d5a998a3aba9867539e98736f5498cd81a1a9166651"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.9/rpi-v0.3.9-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "5f581b6b3bf18647feccf359999dd640a83cf88038ef27c5922814f957baf6d7"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.9/rpi-v0.3.9-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "3195cb78cda90cd9e937a9f3266f850d6c0b074208f3803d0b2c2bf2570c9729"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
