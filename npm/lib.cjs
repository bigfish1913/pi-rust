'use strict';

const { createHash } = require('node:crypto');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');

const repository = 'https://github.com/bigfish1913/pi-rust';

function targetFor(platform = process.platform, arch = process.arch, report) {
  const targets = {
    'darwin-arm64': 'aarch64-apple-darwin',
    'darwin-x64': 'x86_64-apple-darwin',
    'win32-x64': 'x86_64-pc-windows-msvc',
    'linux-arm64': 'aarch64-unknown-linux-gnu',
    'linux-x64': 'x86_64-unknown-linux-gnu',
  };
  let target = targets[`${platform}-${arch}`];
  if (!target) throw new Error(`Unsupported platform: ${platform}/${arch}. See ${repository}#install.`);
  if (platform === 'linux') {
    report ??= process.report.getReport();
    if (!report.header.glibcVersionRuntime) {
      if (arch !== 'x64') throw new Error('Linux ARM64 requires glibc; no musl ARM64 binary is available.');
      target = 'x86_64-unknown-linux-musl';
    }
  }
  return target;
}

function distributionFor(manifest, version, target) {
  if (manifest.schema_version !== 1 || manifest.name !== 'rpi-cli' ||
      manifest.version !== version || manifest.tag !== `v${version}` ||
      manifest.repository !== repository || !/^[a-f0-9]{40}$/.test(manifest.source_commit)) {
    throw new Error('Release manifest does not match this package.');
  }
  const distribution = manifest.targets?.[target];
  const windows = target.endsWith('-windows-msvc');
  const format = windows ? 'zip' : 'tar.gz';
  const binary = windows ? 'rpi.exe' : 'rpi';
  const expectedUrl = `${repository}/releases/download/v${version}/rpi-v${version}-${target}.${format}`;
  if (!distribution || distribution.url !== expectedUrl || distribution.format !== format ||
      !Number.isSafeInteger(distribution.size) || distribution.size <= 0 || distribution.size > 256 * 1024 * 1024 ||
      !/^[a-f0-9]{64}$/.test(distribution.sha256) || distribution.binary?.path !== binary ||
      !/^[a-f0-9]{64}$/.test(distribution.binary.sha256)) {
    throw new Error(`Invalid or missing release distribution for ${target}.`);
  }
  return distribution;
}

function verifyHash(data, expected, label) {
  const actual = createHash('sha256').update(data).digest('hex');
  if (actual !== expected) throw new Error(`SHA-256 mismatch for ${label}.`);
}

function installedBinary() {
  return join(__dirname, 'native', process.platform === 'win32' ? 'rpi.exe' : 'rpi');
}

function verifyBinary(path, distribution) {
  verifyHash(readFileSync(path), distribution.binary.sha256, distribution.binary.path);
}

async function download(url, limit, fetcher = fetch) {
  for (let attempt = 1; attempt <= 3; attempt++) {
    try {
      const response = await fetcher(url, { signal: AbortSignal.timeout(300_000) });
      if (!response.ok) {
        await response.body?.cancel();
        const error = new Error(`Download failed (${response.status}): ${url}`);
        error.retryable = response.status === 408 || response.status === 429 || response.status >= 500;
        throw error;
      }
      const chunks = [];
      let size = 0;
      for await (const chunk of response.body) {
        size += chunk.length;
        if (size > limit) {
          const error = new Error(`Download exceeds expected size: ${url}`);
          error.retryable = false;
          throw error;
        }
        chunks.push(chunk);
      }
      return Buffer.concat(chunks);
    } catch (error) {
      if (error.retryable === false || attempt === 3) throw error;
      console.error(`rpi: download interrupted (${error.cause?.code ?? error.message}); retrying (${attempt}/3)...`);
    }
  }
}

module.exports = { targetFor, distributionFor, verifyHash, installedBinary, verifyBinary, download };
