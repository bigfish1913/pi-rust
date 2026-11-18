import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  AUDIENCE, claimSummary, configurationHints, decodeClaims, exchangeUrl, main, workflowFilename,
} from './npm-oidc-preflight.mjs';

const packageName = '@bigfishnpm/rpi';

function jwt(claims) {
  const encode = value => Buffer.from(JSON.stringify(value)).toString('base64url');
  return `${encode({ alg: 'RS256' })}.${encode(claims)}.signature`;
}

const claims = {
  repository: 'bigfish1913/pi-rust',
  repository_owner: 'bigfish1913',
  workflow_ref: 'bigfish1913/pi-rust/.github/workflows/ci.yml@refs/heads/main',
  job_workflow_ref: 'bigfish1913/pi-rust/.github/workflows/release-npm.yml@refs/heads/main',
  environment: '',
};

// Fake GitHub OIDC endpoint plus npm registry; the id token stays a real JWT so
// claim decoding is exercised rather than stubbed.
function fakeFetch({ exchange, idToken = jwt(claims) }) {
  const calls = [];
  const fetcher = async (url, options = {}) => {
    calls.push({ url: String(url), options });
    if (String(url).startsWith('https://pipelines.actions.example.com/token')) {
      return new Response(JSON.stringify({ value: idToken }), { status: 200 });
    }
    return exchange(String(url), options);
  };
  return { fetcher, calls };
}

const env = {
  ACTIONS_ID_TOKEN_REQUEST_URL: 'https://pipelines.actions.example.com/token?api-version=2',
  ACTIONS_ID_TOKEN_REQUEST_TOKEN: 'request-token',
};

test('preflight stays quiet without a GitHub Actions OIDC context', async () => {
  const seen = [];
  assert.equal(await main({ env: {}, report: text => seen.push(text) }), 'skipped');
  assert.match(seen.join('\n'), /skipping the npm trusted publisher preflight/);
});

test('exchange requests the npm audience and escaped package path', async () => {
  const { fetcher, calls } = fakeFetch({ exchange: () => new Response('{"token":"npm_token"}', { status: 200 }) });
  const seen = [];
  assert.equal(await main({ env, fetcher, report: text => seen.push(text) }), 'accepted');
  assert.match(calls[0].url, /audience=npm%3Aregistry\.npmjs\.org/);
  assert.equal(AUDIENCE, 'npm:registry.npmjs.org');
  assert.equal(exchangeUrl().href, 'https://registry.npmjs.org/-/npm/v1/oidc/token/exchange/package/@bigfishnpm%2Frpi');
  assert.equal(calls[1].url, 'https://registry.npmjs.org/-/npm/v1/oidc/token/exchange/package/@bigfishnpm%2Frpi');
  assert.equal(calls[1].options.method, 'POST');
  assert.equal(calls[1].options.headers.Authorization, `Bearer ${jwt(claims)}`);
  assert.match(seen.at(-1), /Allow npm publish/);
});

test('a rejected exchange names the claims and the workflow filenames', async () => {
  const { fetcher } = fakeFetch({
    exchange: () => new Response('{"error":"Not found"}', { status: 404 }),
  });
  await assert.rejects(main({ env, fetcher, report: () => {} }), error => {
    assert.match(error.message, /HTTP 404/);
    assert.match(error.message, /job_workflow_ref=.*release-npm\.yml/);
    assert.match(error.message, /workflow_ref=.*ci\.yml/);
    assert.match(error.message, /Workflow filename: release-npm\.yml/);
    assert.match(error.message, /Workflow filename: ci\.yml/);
    assert.match(error.message, /Organization or user: bigfish1913/);
    assert.match(error.message, /Repository: pi-rust/);
    assert.match(error.message, /two days after creation/);
    return true;
  });
});

test('a missing id-token permission fails with the required workflow setting', async () => {
  const fetcher = async () => new Response('{"message":"Resource not accessible"}', { status: 403 });
  await assert.rejects(main({ env, fetcher, report: () => {} }), /HTTP 403.*id-token: write/s);
});

test('an exchange without a token is an error, not a silent anonymous publish', async () => {
  for (const body of ['{}', '{"token":""}', 'not json']) {
    const { fetcher } = fakeFetch({ exchange: () => new Response(body, { status: 200 }) });
    await assert.rejects(main({ env, fetcher, report: () => {} }), /returned no publish token|non-JSON body/);
  }
});

test('claims fall back to the caller workflow for non-reusable jobs', () => {
  const plain = claimSummary({ workflow_ref: 'o/r/.github/workflows/release-npm.yml@refs/heads/main' });
  assert.equal(plain.workflow, 'release-npm.yml');
  assert.equal(plain.job_workflow, 'release-npm.yml');
  assert.equal(workflowFilename(null), null);
  assert.equal(workflowFilename('o/r/.github/workflows/x.yaml@refs/tags/v1'), 'x.yaml');
  assert.throws(() => decodeClaims('not-a-jwt'), /not a JWT/);
  assert.match(configurationHints({}).join('\n'), /Allowed actions: select "Allow npm publish"/);
  assert.match(configurationHints({}, packageName).join('\n'), /@bigfishnpm\/rpi/);
});
