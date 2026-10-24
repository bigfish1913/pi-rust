# Homebrew formula for rpi.
#
# Not in homebrew-core: that requires notability (stars/forks) rpi does not have
# yet. It works today from a tap — publish this file as `Formula/rpi.rb` in a
# repo named `homebrew-tap` under the same owner, then:
#
#   brew tap bigfish1913/tap
#   brew install rpi
#
# Hashes below are for v0.5.1. See packaging/README.md for the refresh procedure.
class Rpi < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://rpi.laofu.online/"
  version "0.5.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.5.1/rpi-v0.5.1-aarch64-apple-darwin.tar.gz"
      sha256 "e2591167842e40e09ed1f3bc0904787f2ecbd8adb784e9c637b7a24611ee49a5"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.5.1/rpi-v0.5.1-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "f8eb9e5a3973993516f0575ce75705d807d971accfca3f2fb691922615e567b3"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.5.1/rpi-v0.5.1-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "45a638aaf6cbefb3304174c687069abd3ed5fd89fcc178151f7a3fc8db9617ef"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
