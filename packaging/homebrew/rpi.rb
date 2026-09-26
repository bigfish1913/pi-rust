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
  version "0.3.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.2/rpi-v0.3.2-aarch64-apple-darwin.tar.gz"
      sha256 "92e1851f5987d99899653e52e120d21fa57f49d5ccd1c7d44e8bc7d9de5b1842"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.2/rpi-v0.3.2-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "2a940c245ba029d47421e6adb7adafb4fd6018853d37fe9e8da07e53d1438f3f"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.2/rpi-v0.3.2-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "b943b36414decd5af9c6b94115ab5fb69ec1780ad60fe7533b8d985f7ec8fa23"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
