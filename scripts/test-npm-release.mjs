import assert from 'node:assert/strict';
import { test } from 'node:test';
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { publicationPlan, registryVersion } from './publish-npm.mjs';

const name = '@bigfishnpm/rpi';
const repository = { url: 'git+https://github.com/bigfish1913/pi-rust.git' };
const record = version => ({ name, version, repository });

test('retry skips an immutable published version without moving tags', () => {
  assert.deepEqual(publicationPlan('0.22.1', record('0.22.1'), record('0.24.0')), { skip: true });
  assert.throws(() => publicationPlan('0.22.1', record('0.22.0'), null), /does not match/);
  assert.throws(() => publicationPlan('0.22.1', { ...record('0.22.1'), repository: { url: 'other' } }, null), /does not match/);
});

test('late release backfills never downgrade latest across major, minor or patch', () => {
  for (const [candidate, latest] of [['0.22.1', '0.23.0'], ['1.9.0', '2.0.0'], ['0.22.1', '0.22.2']]) {
    assert.deepEqual(publicationPlan(candidate, null, record(latest)), { skip: false, tag: 'backfill' });
  }
  for (const [candidate, latest] of [['0.23.0', '0.22.9'], ['1.0.0', '0.99.0'], ['0.22.10', '0.22.9']]) {
    assert.deepEqual(publicationPlan(candidate, null, record(latest)), { skip: false, tag: 'latest' });
  }
  assert.deepEqual(publicationPlan('0.22.1', null, null), { skip: false, tag: 'latest' });
});

test('npm stable channel rejects prereleases and invalid versions', () => {
  for (const version of ['0.22.1-preview.1', 'v0.22.1', '01.2.3', '1.2', '', '1.2.3; echo injected']) {
    assert.throws(() => publicationPlan(version, null, null), /stable release/);
  }
});

test('only a registry 404 means that a version is absent', async () => {
  assert.equal(await registryVersion('0.22.1', async () => new Response('', { status: 404 })), null);
  for (const status of [401, 403, 429, 500, 503]) {
    await assert.rejects(registryVersion('0.22.1', async () => new Response('', { status })), /lookup failed/);
  }
  await assert.rejects(registryVersion('0.22.1', async () => { throw new Error('connection reset'); }), /connection reset/);
});

test('registry lookup uses the version endpoint even during metadata propagation', async () => {
  const expected = record('0.22.1');
  const result = await registryVersion('0.22.1', async url => {
    assert.equal(url, 'https://registry.npmjs.org/@bigfishnpm%2frpi/0.22.1');
    return Response.json(expected);
  });
  assert.deepEqual(result, expected);
  await assert.rejects(registryVersion('0.22.1', async () => Response.json(record('0.22.0'))), /unexpected/);
});

async function prepareFixture(t) {
  const directory = await mkdtemp(join(tmpdir(), 'rpi-npm-release-'));
  t.after(() => rm(directory, { recursive: true, force: true }));
  await mkdir(join(directory, 'scripts'));
  await mkdir(join(directory, 'npm'));
  const root = new URL('../', import.meta.url);
  for (const path of ['scripts/prepare-npm.mjs', 'npm/lib.cjs', 'LICENSE', 'NOTICE']) {
    await copyFile(new URL(path, root), join(directory, path));
  }
  const manifest = JSON.parse(await readFile(new URL('npm/release.json', root), 'utf8'));
  const pkg = JSON.parse(await readFile(new URL('package.json', root), 'utf8'));
  pkg.version = '99.0.0';
  await writeFile(join(directory, 'package.json'), JSON.stringify(pkg));
  await writeFile(join(directory, 'Cargo.toml'), '[workspace.package]\nversion = "99.0.0"\n');
  const manifestPath = join(directory, 'provided-release.json');
  await writeFile(manifestPath, JSON.stringify(manifest));
  return { directory, manifestPath, manifest };
}

test('preparation pins an older requested release independently of the main workspace version', async t => {
  const { directory, manifestPath, manifest } = await prepareFixture(t);
  for (let attempt = 0; attempt < 2; attempt++) {
    const result = spawnSync(process.execPath, ['scripts/prepare-npm.mjs'], {
      cwd: directory, encoding: 'utf8',
      env: { ...process.env, RPI_NPM_RELEASE_VERSION: manifest.version, RPI_NPM_RELEASE_MANIFEST: manifestPath },
    });
    assert.equal(result.status, 0, result.stderr);
  }
  const pkg = JSON.parse(await readFile(join(directory, 'package.json'), 'utf8'));
  assert.equal(pkg.version, manifest.version);
  assert.deepEqual(JSON.parse(await readFile(join(directory, 'npm/release.json'), 'utf8')), manifest);
});

test('preparation fails before writing when the supplied manifest has a different version', async t => {
  const { directory, manifestPath } = await prepareFixture(t);
  const before = await readFile(join(directory, 'package.json'), 'utf8');
  const result = spawnSync(process.execPath, ['scripts/prepare-npm.mjs'], {
    cwd: directory, encoding: 'utf8',
    env: { ...process.env, RPI_NPM_RELEASE_VERSION: '98.0.0', RPI_NPM_RELEASE_MANIFEST: manifestPath },
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /does not match the requested version/);
  assert.equal(await readFile(join(directory, 'package.json'), 'utf8'), before);
});
