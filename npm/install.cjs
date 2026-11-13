'use strict';

const { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } = require('node:fs');
const { tmpdir } = require('node:os');
const { dirname, join } = require('node:path');
const { spawnSync } = require('node:child_process');
const { distributionFor, download, installedBinary, targetFor, verifyBinary, verifyHash } = require('./lib.cjs');

async function install() {
  const { version } = require('../package.json');
  const manifest = JSON.parse(readFileSync(join(__dirname, 'release.json'), 'utf8'));
  const target = targetFor();
  const distribution = distributionFor(manifest, version, target);
  const destination = installedBinary();
  if (existsSync(destination)) {
    try {
      verifyBinary(destination, distribution);
      return;
    } catch {
      // Replace an incomplete or outdated installation only after verification.
    }
  }
  console.error(`@bigfishnpm/rpi: downloading rpi ${version} (${target})...`);
  const temporary = mkdtempSync(join(tmpdir(), 'pi-rust-'));
  let staged;
  try {
    const archive = join(temporary, `release.${distribution.format}`);
    const data = await download(distribution.url, distribution.size);
    if (data.length !== distribution.size) throw new Error('Downloaded archive has the wrong size.');
    verifyHash(data, distribution.sha256, 'release archive');
    writeFileSync(archive, data);
    let extraction;
    if (process.platform === 'win32') {
      // PowerShell is present on supported Windows hosts and handles ZIP files
      // even when a GNU tar from Git happens to precede Windows tar on PATH.
      extraction = spawnSync('powershell.exe', [
        '-NoLogo', '-NoProfile', '-NonInteractive', '-Command',
        '$ErrorActionPreference = "Stop"; Expand-Archive -LiteralPath $env:RPI_NPM_ARCHIVE -DestinationPath $env:RPI_NPM_EXTRACT',
      ], {
        stdio: 'inherit',
        env: { ...process.env, RPI_NPM_ARCHIVE: archive, RPI_NPM_EXTRACT: temporary },
      });
    } else {
      extraction = spawnSync('tar', ['-xzf', archive, '-C', temporary, `./${distribution.binary.path}`], { stdio: 'inherit' });
    }
    if (extraction.error) throw extraction.error;
    if (extraction.status !== 0) throw new Error('Could not extract the release archive.');
    const extracted = join(temporary, distribution.binary.path);
    verifyBinary(extracted, distribution);
    chmodSync(extracted, 0o755);
    mkdirSync(dirname(destination), { recursive: true });
    staged = `${destination}.${process.pid}.tmp`;
    copyFileSync(extracted, staged);
    chmodSync(staged, 0o755);
    renameSync(staged, destination);
  } finally {
    if (staged) rmSync(staged, { force: true });
    rmSync(temporary, { recursive: true, force: true });
  }
}

if (require.main === module) {
  install().catch(error => {
    console.error(`@bigfishnpm/rpi: ${error.message}`);
    process.exitCode = 1;
  });
}

module.exports = { install };
