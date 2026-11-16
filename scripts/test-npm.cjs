'use strict';

const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const { test } = require('node:test');
const { targetFor, distributionFor, verifyHash, download } = require('../npm/lib.cjs');
const manifest = require('../npm/release.json');
const { version } = require('../package.json');

test('selects each supported native distribution, including Linux musl', () => {
  const glibc = { header: { glibcVersionRuntime: '2.39' } };
  const musl = { header: {} };
  assert.equal(targetFor('darwin', 'arm64'), 'aarch64-apple-darwin');
  assert.equal(targetFor('darwin', 'x64'), 'x86_64-apple-darwin');
  assert.equal(targetFor('win32', 'x64'), 'x86_64-pc-windows-msvc');
  assert.equal(targetFor('linux', 'x64', glibc), 'x86_64-unknown-linux-gnu');
  assert.equal(targetFor('linux', 'arm64', glibc), 'aarch64-unknown-linux-gnu');
  assert.equal(targetFor('linux', 'x64', musl), 'x86_64-unknown-linux-musl');
  assert.throws(() => targetFor('linux', 'arm64', musl), /requires glibc/);
  assert.throws(() => targetFor('win32', 'arm64'), /Unsupported platform/);
});

test('pinned release matches package version and contains all six targets', () => {
  assert.equal(Object.keys(manifest.targets).length, 6);
  for (const target of Object.keys(manifest.targets)) {
    assert.equal(distributionFor(manifest, version, target), manifest.targets[target]);
  }
});

test('rejects changed publisher, version, URL, binary path and checksums', () => {
  const target = 'x86_64-pc-windows-msvc';
  const mutations = [
    copy => { copy.version = '999.0.0'; },
    copy => { copy.repository = 'https://example.com'; },
    copy => { copy.targets[target].url = 'https://example.com/rpi.zip'; },
    copy => { copy.targets[target].binary.path = '../rpi.exe'; },
    copy => { copy.targets[target].binary.sha256 = 'invalid'; },
    copy => { copy.targets[target].size = -1; },
  ];
  for (const mutate of mutations) {
    const copy = structuredClone(manifest);
    mutate(copy);
    assert.throws(() => distributionFor(copy, version, target));
  }
});

test('rejects archive or executable content with a different digest', () => {
  const bytes = Buffer.from('verified release');
  const digest = createHash('sha256').update(bytes).digest('hex');
  assert.doesNotThrow(() => verifyHash(bytes, digest, 'archive'));
  assert.throws(() => verifyHash(Buffer.from('modified release'), digest, 'archive'), /SHA-256 mismatch/);
});

test('download retries transient connection failures and server errors', async () => {
  let attempts = 0;
  const bytes = await download('https://example.com/release', 2, async () => {
    attempts++;
    if (attempts === 1) throw new TypeError('connection reset');
    if (attempts === 2) return new Response('unavailable', { status: 503 });
    return new Response('ok');
  });
  assert.equal(bytes.toString(), 'ok');
  assert.equal(attempts, 3);
});

test('download rejects oversized content and missing releases without retrying', async () => {
  for (const [response, message] of [
    [() => new Response('too large'), /exceeds expected size/],
    [() => new Response('missing', { status: 404 }), /Download failed \(404\)/],
  ]) {
    let attempts = 0;
    await assert.rejects(download('https://example.com/release', 2, async () => {
      attempts++;
      return response();
    }), message);
    assert.equal(attempts, 1);
  }
});
