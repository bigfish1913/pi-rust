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
  version "0.3.19"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.19/rpi-v0.3.19-aarch64-apple-darwin.tar.gz"
      sha256 "25e334e183614442fe7eb3ea56e9f17a87e0c745d1bc841001f76d18eb6a2b51"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.19/rpi-v0.3.19-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "d65249cb46f072d8e0e5932ed50ed1b348cb046ff1c4789b3bd0a57dc1671803"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.19/rpi-v0.3.19-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "c48ef652be3093a28f06076e5a21f2746e155959a181aadd2d17dbdb819991bd"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
