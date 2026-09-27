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
  version "0.3.4"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.4/rpi-v0.3.4-aarch64-apple-darwin.tar.gz"
      sha256 "73fece5f8d7763baab84275ea1869808a57f4036f0755fecfdad43cafaebc110"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.4/rpi-v0.3.4-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "1adf7070a431b87f7fbb19bab0815ca125b79379ca8553acda0079469b664fc4"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.4/rpi-v0.3.4-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "f1cce61d79610731c440db91eb167139481db27186b0b2985fda469a3390be86"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
