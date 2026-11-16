#!/usr/bin/env node
import { appendFile, readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { setTimeout as pause } from 'node:timers/promises';

const name = '@bigfishnpm/rpi';
const repository = 'git+https://github.com/bigfish1913/pi-rust.git';

function stableVersion(version) {
  if (typeof version !== 'string' || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)) {
    throw new Error(`Expected a stable release version, got ${version}.`);
  }
  return version.split('.').map(BigInt);
}

export function publicationPlan(version, published, latest) {
  const parts = stableVersion(version);
  if (published) {
    if (published.name !== name || published.version !== version || published.repository?.url !== repository) {
      throw new Error('Existing registry version does not match this package.');
    }
    return { skip: true };
  }
  if (latest) {
    const latestParts = stableVersion(latest.version);
    for (let i = 0; i < parts.length; i++) {
      if (parts[i] < latestParts[i]) return { skip: false, tag: 'backfill' };
      if (parts[i] > latestParts[i]) break;
    }
  }
  return { skip: false, tag: 'latest' };
}

export async function registryVersion(version, fetcher = fetch) {
  const response = await fetcher(`https://registry.npmjs.org/@bigfishnpm%2frpi/${encodeURIComponent(version)}`, {
    signal: AbortSignal.timeout(15_000),
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`npm registry lookup failed: HTTP ${response.status}.`);
  const record = await response.json();
  if (record.name !== name || !record.version || (version !== 'latest' && record.version !== version)) {
    throw new Error('npm registry returned an unexpected package/version.');
  }
  return record;
}

async function main() {
  const root = new URL('../', import.meta.url);
  const pkg = JSON.parse(await readFile(new URL('package.json', root), 'utf8'));
  if (pkg.name !== name || pkg.repository?.url !== repository) throw new Error('Unexpected npm package identity.');
  stableVersion(pkg.version);
  const published = await registryVersion(pkg.version);
  const latest = published ? null : await registryVersion('latest');
  const plan = publicationPlan(pkg.version, published, latest);
  const report = async text => {
    console.log(text);
    if (process.env.GITHUB_STEP_SUMMARY) await appendFile(process.env.GITHUB_STEP_SUMMARY, `${text}\n`);
  };
  if (plan.skip) {
    await report(`${name}@${pkg.version} is already published; skipping without changing dist-tags.`);
    return;
  }
  const dryRun = process.argv.includes('--dry-run');
  const args = ['publish', '--access', 'public', '--provenance', '--tag', plan.tag];
  if (dryRun) args.push('--dry-run');
  const result = spawnSync('npm', args, {
    cwd: fileURLToPath(root), stdio: 'inherit', shell: process.platform === 'win32',
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`npm publish failed with exit code ${result.status}.`);
  if (dryRun) return;
  // New publications can return HTTP 202 before package metadata is ready.
  // The version endpoint becomes available before the full package document.
  for (let attempt = 0; attempt < 30; attempt++) {
    const record = await registryVersion(pkg.version);
    if (record) {
      publicationPlan(pkg.version, record, null);
      await report(`Published ${name}@${pkg.version} (dist-tag: ${plan.tag}).`);
      return;
    }
    await pause(10_000);
  }
  throw new Error('npm accepted the publication but its version is still processing. Check the registry before retrying.');
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  main().catch(error => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
