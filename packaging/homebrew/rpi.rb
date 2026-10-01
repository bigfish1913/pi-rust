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
  version "0.3.10"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.10/rpi-v0.3.10-aarch64-apple-darwin.tar.gz"
      sha256 "14d38483177929770f8d3f8f1e97ecf814133e510816ca452373fead738cc970"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.10/rpi-v0.3.10-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "11bfd9b710e3e086c56133d0fa1e6d897cbfc3aa5143f3ffd19f1ca4e25b51ae"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.10/rpi-v0.3.10-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ff2289ad4adaeda60babc4ce122c739680695bb66ab5c7e6e3a33b2f7897fd8c"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
