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
  version "0.3.3"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.3/rpi-v0.3.3-aarch64-apple-darwin.tar.gz"
      sha256 "1b79f294bec0bea18ff4191a9d255dc5b6402ddeed2323efbd4aaf752dc88691"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.3/rpi-v0.3.3-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "2eb3371d7aeadd6f6101fdfd3bc3369d5a8325ff8d1c86c99b979ce9e45069e1"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.3/rpi-v0.3.3-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "de161048f1426be41d891e5593d24c2aedb4500ecf5ea55e0f55bc2d2fad88fe"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
