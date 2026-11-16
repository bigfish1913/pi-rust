#!/usr/bin/env node
import { readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { distributionFor, download } = require('../npm/lib.cjs');
const root = new URL('../', import.meta.url);
const cargo = await readFile(new URL('Cargo.toml', root), 'utf8');
const workspace = cargo.split('[workspace.package]')[1]?.split('\n[')[0];
// CI may retry an older stable tag using the reviewed packaging code on main.
const version = process.env.RPI_NPM_RELEASE_VERSION ?? workspace?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!version || !/^\d+\.\d+\.\d+(?:-[\da-zA-Z.-]+)?$/.test(version)) {
  throw new Error('Could not determine the workspace release version.');
}
const packagePath = new URL('package.json', root);
const pkg = JSON.parse(await readFile(packagePath, 'utf8'));
const repository = 'https://github.com/bigfish1913/pi-rust';
const url = `${repository}/releases/download/v${version}/rpi-release.json`;
const manifestPath = new URL('npm/release.json', root);
let manifest;
try {
  manifest = JSON.parse(await readFile(process.env.RPI_NPM_RELEASE_MANIFEST || manifestPath, 'utf8'));
} catch (error) {
  if (error.code !== 'ENOENT' || process.env.RPI_NPM_RELEASE_MANIFEST) throw error;
}
if (process.env.RPI_NPM_RELEASE_MANIFEST && manifest.version !== version) {
  throw new Error('Provided release manifest does not match the requested version.');
}
if (!manifest || manifest.version !== version || process.argv.includes('--refresh')) {
  manifest = JSON.parse(await download(url, 1024 * 1024));
}
const targets = [
  'aarch64-apple-darwin', 'x86_64-apple-darwin',
  'x86_64-pc-windows-msvc', 'aarch64-unknown-linux-gnu',
  'x86_64-unknown-linux-gnu', 'x86_64-unknown-linux-musl',
];
for (const target of targets) distributionFor(manifest, version, target);
await Promise.all(['LICENSE', 'NOTICE'].map(name => readFile(new URL(name, root))));
pkg.version = version;
await writeFile(packagePath, `${JSON.stringify(pkg, null, 2)}\n`);
await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`Prepared ${pkg.name}@${version} with ${targets.length} verified distribution descriptors.`);
console.log(`Pinned release manifest: ${fileURLToPath(manifestPath)}`);
