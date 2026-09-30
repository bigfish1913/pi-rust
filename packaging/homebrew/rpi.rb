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
  version "0.3.6"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.6/rpi-v0.3.6-aarch64-apple-darwin.tar.gz"
      sha256 "147cf807d16d400f345cbf8bfcd0f823bb7073fef00027999db58665e143c4f9"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.6/rpi-v0.3.6-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "2c7616d0a6fcd068750b0319fe54447b346508bd9d9b48c6e59511e25cfabcbf"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.6/rpi-v0.3.6-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ba7c5c05cbfbabe96c78b74e384c287aadfc23f28034dbcef59bd5b20f8a7b12"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
