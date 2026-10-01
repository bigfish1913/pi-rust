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
  version "0.3.11"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.11/rpi-v0.3.11-aarch64-apple-darwin.tar.gz"
      sha256 "0c2b0c4a57586d71d4ea635b6f8a823ea62e746c740582d7cad6607821ee7b98"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.11/rpi-v0.3.11-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "db8727ca7fba01c6ff9feb5780358920f2fa780a42dc0229535b03504daade14"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.11/rpi-v0.3.11-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "048893c6034c85dccc4d8174acb86060a4679799acce72c5dde954a98f20a14f"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
