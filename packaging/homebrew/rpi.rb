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
  version "0.3.13"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.13/rpi-v0.3.13-aarch64-apple-darwin.tar.gz"
      sha256 "bd52b26a23a8aa418adbd1f1496edd3ded14582110ac4ee30549065305894100"
    end
    # No x86_64 macOS build is published (see .github/workflows/release-binaries.yml).
    # Intel Macs are covered by `cargo install rpi-cli`.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.13/rpi-v0.3.13-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "49c1ffe935216138341621984e21d6f093664ba092ad7f9eef6b4b41fbe34f96"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.13/rpi-v0.3.13-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "7ba1ac0e4726ade66369e3f3bc365c6ac692be29fe8b20c8f0723cf834118534"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
