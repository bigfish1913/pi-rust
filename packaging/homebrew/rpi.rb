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
  version "0.3.17"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.17/rpi-v0.3.17-aarch64-apple-darwin.tar.gz"
      sha256 "5da148e32e7c1a3c58790adf54fca9a0370317e1c35dacda37d6309fa6bb1518"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.17/rpi-v0.3.17-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "f0d3435331459d8fb236f8297f72c8a5be306bcd0c1f5b19a4127e2d86c005b0"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.17/rpi-v0.3.17-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "f3eae2e20601a7c76b39e9e532e9dea93887eb4258b589d93d65c7f49cff2c84"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
