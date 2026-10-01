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
  version "0.3.12"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.12/rpi-v0.3.12-aarch64-apple-darwin.tar.gz"
      sha256 "590f85722b9aab48fdbbc73188658df08a58893bbcae8f26b3f76b86873d6898"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.12/rpi-v0.3.12-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "fe2cfedf76d25bb8dfb0455838b52da83f13d2dbfbb5c6e45dcc3e7a418bf30e"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.12/rpi-v0.3.12-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "326149d98c50e236d1b959cf1cb58b503c160338d3f6087b5c5fe7b6a9d2593e"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
