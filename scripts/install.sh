#!/bin/sh
# Install the prebuilt `rpi` binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
#
# Options (pass after `sh -s --`, or export as env vars):
#   --version <v>     Install a specific version, e.g. --version 0.3.0 (default: latest)
#   --dir <path>      Install into <path> instead of ~/.local/bin or /usr/local/bin
#   --dry-run         Print what would be downloaded, then exit
#
# If you would rather build from source, use `cargo install rpi-cli`.

set -eu

REPO="bigfish1913/pi-rust"
BIN="rpi"
VERSION=""
INSTALL_DIR=""
DRY_RUN=0

log() { printf '%s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --dir) INSTALL_DIR="${2:-}"; shift 2 ;;
    --dir=*) INSTALL_DIR="${1#*=}"; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help)
      sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

# --- platform detection ------------------------------------------------------

os="$(uname -s)"
arch="$(uname -m)"

case "${os}" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  MINGW*|MSYS*|CYGWIN*) os_part="pc-windows-msvc" ;;
  *) die "unsupported operating system: ${os}. Build from source with 'cargo install rpi-cli'." ;;
esac

case "${arch}" in
  x86_64|amd64) arch_part="x86_64" ;;
  aarch64|arm64) arch_part="aarch64" ;;
  *) die "unsupported architecture: ${arch}. Build from source with 'cargo install rpi-cli'." ;;
esac

# Only the combinations the release workflow actually builds.
case "${arch_part}-${os_part}" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|aarch64-apple-darwin|x86_64-pc-windows-msvc) ;;
  x86_64-apple-darwin)
    die "no prebuilt binary for Intel macOS yet. Build from source with 'cargo install rpi-cli'." ;;
  *) die "no prebuilt binary for ${arch_part}-${os_part}. Build from source with 'cargo install rpi-cli'." ;;
esac

target="${arch_part}-${os_part}"

# --- downloader --------------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1"; }
  download() { curl -fsSL -o "$2" "$1"; }
  download_optional() { curl -fsSL -o "$2" "$1" 2>/dev/null || return 1; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO- "$1"; }
  download() { wget -qO "$2" "$1"; }
  download_optional() { wget -qO "$2" "$1" 2>/dev/null || return 1; }
else
  die "neither curl nor wget is available"
fi

# --- resolve version ---------------------------------------------------------

if [ -z "${VERSION}" ]; then
  VERSION="$(fetch "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\([^"]*\)".*/\1/p' \
    | head -n 1)"
  [ -n "${VERSION}" ] || die "could not determine the latest release version; pass --version"
fi
VERSION="${VERSION#v}"

if [ "${os_part}" = "pc-windows-msvc" ]; then
  asset="${BIN}-v${VERSION}-${target}.zip"
else
  asset="${BIN}-v${VERSION}-${target}.tar.gz"
fi

url="https://github.com/${REPO}/releases/download/v${VERSION}/${asset}"

if [ "${DRY_RUN}" -eq 1 ]; then
  log "platform: ${target}"
  log "version:  ${VERSION}"
  log "url:      ${url}"
  exit 0
fi

# --- extract -----------------------------------------------------------------

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT INT TERM

log "Downloading ${asset}"
download "${url}" "${tmp}/${asset}" \
  || die "download failed: ${url}
Check that v${VERSION} has binaries attached: https://github.com/${REPO}/releases"

case "${asset}" in
  *.zip) command -v unzip >/dev/null 2>&1 || die "unzip is required to install on Windows" ;;
  *) if ! command -v tar >/dev/null 2>&1; then die "tar is required"; fi ;;
esac

if ! download_optional "${url}.sha256" "${tmp}/${asset}.sha256"; then
  log "note: no checksum published for ${asset}; skipping verification"
fi
if [ -f "${tmp}/${asset}.sha256" ] && command -v sha256sum >/dev/null 2>&1; then
  ( cd "${tmp}" && sha256sum -c "${asset}.sha256" ) || die "checksum verification failed"
fi

if [ "${os_part}" = "pc-windows-msvc" ]; then
  ( cd "${tmp}" && unzip -q "${asset}" )
else
  ( cd "${tmp}" && tar -xzf "${asset}" )
fi

src="${tmp}/${BIN}"
[ -f "${src}" ] || src="${tmp}/${BIN}.exe"
[ -f "${src}" ] || die "archive did not contain the ${BIN} binary"

# --- install -----------------------------------------------------------------

if [ -z "${INSTALL_DIR}" ]; then
  if [ -w /usr/local/bin ] 2>/dev/null; then
    INSTALL_DIR="/usr/local/bin"
  else
    INSTALL_DIR="${HOME}/.local/bin"
  fi
fi
mkdir -p "${INSTALL_DIR}"

dest="${INSTALL_DIR}/${BIN}"
if [ "${os_part}" = "pc-windows-msvc" ]; then
  dest="${dest}.exe"
fi

if command -v install >/dev/null 2>&1; then
  install -m 755 "${src}" "${dest}"
else
  cp "${src}" "${dest}"
  chmod 755 "${dest}" 2>/dev/null || true
fi

log "Installed ${BIN} ${VERSION} to ${dest}"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    log ""
    log "Note: ${INSTALL_DIR} is not on your PATH. Add it with:"
    log "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac

"${dest}" --version >&2 2>/dev/null || true
