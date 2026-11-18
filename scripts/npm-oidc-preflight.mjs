#!/usr/bin/env node
// Verify the npm trusted publisher binding before a release spends time packaging.
//
// npm treats OIDC as best effort: when the registry rejects the token exchange it
// silently falls back to an anonymous publish, and the registry answers that with
// a bare `404 PUT /@scope%2Fname`. The log then shows a signed provenance
// statement followed by "404 Not found", which names no cause. This preflight runs
// the same exchange the CLI does, so a misconfigured trusted publisher fails here
// with the OIDC claims npm compared and the checklist that fixes it.
import { appendFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const PACKAGE = '@bigfishnpm/rpi';
export const REGISTRY = 'https://registry.npmjs.org';
export const AUDIENCE = `npm:${new URL(REGISTRY).hostname}`;

export function exchangeUrl(registry = REGISTRY, packageName = PACKAGE) {
  // npm resolves the package through npa and only escapes the scope separator.
  return new URL(`/-/npm/v1/oidc/token/exchange/package/${packageName.replace('/', '%2F')}`, registry);
}

export function decodeClaims(idToken) {
  const parts = String(idToken ?? '').split('.');
  if (parts.length < 2 || !parts[1]) throw new Error('OIDC id_token is not a JWT.');
  const payload = parts[1].replace(/-/g, '+').replace(/_/g, '/');
  const claims = JSON.parse(Buffer.from(payload, 'base64').toString('utf8'));
  if (!claims || typeof claims !== 'object') throw new Error('OIDC id_token payload is not an object.');
  return claims;
}

export function workflowFilename(ref) {
  if (typeof ref !== 'string' || !ref) return null;
  const filename = ref.split('@')[0].split('/').pop();
  return filename || null;
}

// npm compares the claims below with the package's trusted publisher entries.
export function claimSummary(claims = {}) {
  const workflowRef = claims.workflow_ref ?? null;
  // A job defined by a reusable workflow reports that file; plain jobs report the caller.
  const jobWorkflowRef = claims.job_workflow_ref ?? workflowRef;
  return {
    repository: claims.repository ?? null,
    repository_owner: claims.repository_owner ?? null,
    workflow_ref: workflowRef,
    job_workflow_ref: jobWorkflowRef,
    workflow: workflowFilename(workflowRef),
    job_workflow: workflowFilename(jobWorkflowRef),
    environment: claims.environment ?? null,
  };
}

export function configurationHints(claims = {}, packageName = PACKAGE) {
  const summary = claimSummary(claims);
  const repository = summary.repository ?? '<owner>/<repo>';
  const [owner, repo] = repository.includes('/') ? repository.split('/') : [summary.repository_owner ?? '<owner>', '<repo>'];
  // The publishing job lives in the reusable workflow, so that filename is the
  // primary entry; npm allows several entries per package, so adding the caller
  // files costs nothing and covers either comparison rule.
  const filenames = [...new Set([summary.job_workflow, summary.workflow, 'release-npm.yml'].filter(Boolean))];
  return [
    `npm rejected this workflow's OIDC token for ${packageName}.`,
    `Package settings: https://www.npmjs.com/package/${packageName}/access`,
    'Add a GitHub Actions trusted publisher for each workflow filename below:',
    ...filenames.map(name => `  - Workflow filename: ${name}`),
    `  - Organization or user: ${owner}`,
    `  - Repository: ${repo}`,
    '  - Environment name: leave empty unless the publishing job declares one',
    '  - Allowed actions: select "Allow npm publish"; configurations created after',
    '    2026-09-03 allow only staged publishing until that is selected',
    'An unvalidated configuration expires two days after creation, and publishing an',
    'already-published version never validates it. Configure it, then rerun',
    '`Release npm package` for the pending stable tag before that window closes.',
  ];
}

function registryMessage(body) {
  const text = typeof body === 'string' ? body.trim() : '';
  if (!text) return 'the registry returned an empty body';
  try {
    const parsed = JSON.parse(text);
    return parsed.error?.message ?? parsed.error ?? parsed.message ?? text;
  } catch {
    return text;
  }
}

async function requestIdToken({ requestUrl, requestToken, fetcher, audience }) {
  const url = new URL(requestUrl);
  url.searchParams.append('audience', audience);
  const response = await fetcher(url.href, {
    headers: { Accept: 'application/json', Authorization: `Bearer ${requestToken}` },
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`GitHub OIDC token request failed: HTTP ${response.status} ${registryMessage(text)}. `
      + 'The publishing job needs `permissions: id-token: write`.');
  }
  const payload = JSON.parse(text);
  if (typeof payload.value !== 'string' || !payload.value) {
    throw new Error('GitHub OIDC token response is missing its value; check `id-token: write`.');
  }
  return payload.value;
}

export async function main({ env = process.env, fetcher = fetch, report = console.log } = {}) {
  const requestUrl = env.ACTIONS_ID_TOKEN_REQUEST_URL;
  const requestToken = env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
  if (!requestUrl || !requestToken) {
    await report('No GitHub Actions OIDC context (ACTIONS_ID_TOKEN_REQUEST_*); skipping the npm trusted publisher preflight.');
    return 'skipped';
  }
  const idToken = await requestIdToken({
    requestUrl, requestToken, fetcher, audience: env.RPI_NPM_OIDC_AUDIENCE || AUDIENCE,
  });
  const claims = claimSummary(decodeClaims(idToken));
  const response = await fetcher(exchangeUrl(), {
    method: 'POST',
    headers: { Authorization: `Bearer ${idToken}` },
  });
  const text = await response.text();
  if (!response.ok) {
    const details = [
      `npm refused the OIDC token exchange: HTTP ${response.status} ${registryMessage(text)}`,
      `OIDC claims: repository=${claims.repository} workflow_ref=${claims.workflow_ref} `
        + `job_workflow_ref=${claims.job_workflow_ref} environment=${claims.environment ?? ''}`,
      ...configurationHints(claims),
    ];
    if (env.GITHUB_STEP_SUMMARY) await appendFile(env.GITHUB_STEP_SUMMARY, `${details.join('\n')}\n`);
    throw new Error(details.join('\n'));
  }
  let payload;
  try {
    payload = JSON.parse(text);
  } catch {
    throw new Error(`npm accepted the OIDC exchange with a non-JSON body: ${registryMessage(text)}`);
  }
  if (typeof payload?.token !== 'string' || !payload.token) {
    throw new Error('npm accepted the OIDC exchange but returned no publish token.');
  }
  await report(`npm trusted publisher accepted this workflow (${claims.job_workflow} for ${claims.repository}).`);
  await report('If the publish step still fails, re-check "Allow npm publish" on the same configuration.');
  return 'accepted';
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  main().catch(error => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
