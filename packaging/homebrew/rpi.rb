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
  version "0.3.15"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.15/rpi-v0.3.15-aarch64-apple-darwin.tar.gz"
      sha256 "24a6ad0bd24d0acd8e69008b4728827326d52c3c063a773a777f54960d08109c"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.15/rpi-v0.3.15-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "d949e121cb7034f159e81822e1cadee639d10bfbcbeee003622c6905317ecc66"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.15/rpi-v0.3.15-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "df3ef241c1efea0edd3e3e4fe0ce8de88fba66c187bd73aeadec18f1fbded027"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
