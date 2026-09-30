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
  version "0.3.5"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.5/rpi-v0.3.5-aarch64-apple-darwin.tar.gz"
      sha256 "09cec8e2ea81b3a36eb008f2a854a3860ca26c31c4183eb45916c51d609bc83b"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.5/rpi-v0.3.5-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "567d639eef522f4940c1cc8a303b5d9c7e16b10d06a3b0ce6949fc5221c9fcb0"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.5/rpi-v0.3.5-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "4dfd6c7a12a282befe6b3162e1a4699d1406e4096648358929e7828f376df245"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
