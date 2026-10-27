// node --test tools/corpus-hub-site/test_hub_add_repos.mjs  (Node 18+; no dependencies)
// /api/v1/me/repos: a signed-in GitHub owner adds (and withdraws) their own repositories.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeSessionCookie, SESSION_COOKIE } from '../../functions/_lib/session.js';
import { onRequestGet, onRequestPost, checkRepos, objectKey, CONSENT_VERSION } from '../../functions/api/v1/me/repos.js';
import { onRequestGet as me } from '../../functions/api/me.js';

const SECRET = 'x'.repeat(40);
const ANN = { provider: 'github', id: '42', login: 'Ann', name: 'Ann', avatar: '' };
const HF = { provider: 'hf', id: 'h1', login: 'ann', name: 'Ann', avatar: '' };

function bucket() {
  const m = new Map();
  return {
    m,
    async put(k, v) { m.set(k, v); },
    async get(k) { return m.has(k) ? { text: async () => m.get(k) } : null; },
    async list({ prefix }) { return { objects: [...m.keys()].filter((k) => k.startsWith(prefix)).sort().map((key) => ({ key })), truncated: false }; },
  };
}
const ENV = (b) => ({ HUB_SESSION_SECRET: SECRET, GITHUB_OAUTH_CLIENT_ID: 'i', GITHUB_OAUTH_CLIENT_SECRET: 's', HUB_REPO_REQUESTS: b });
async function cookieFor(u) {
  const sc = await makeSessionCookie(SECRET, u);
  return `${SESSION_COOKIE}=${sc.match(new RegExp(`${SESSION_COOKIE}=([^;]*)`))[1]}`;
}
async function post(body, { user = ANN, origin = 'https://hub.xerj.org', ct = 'application/json', env } = {}) {
  const headers = { 'content-type': ct };
  if (user) headers.cookie = await cookieFor(user);
  if (origin) headers.origin = origin;
  return onRequestPost({ request: new Request('https://hub.xerj.org/api/v1/me/repos', { method: 'POST', headers, body: JSON.stringify(body) }), env });
}
async function get(env, user = ANN) {
  const headers = user ? { cookie: await cookieFor(user) } : {};
  return onRequestGet({ request: new Request('https://hub.xerj.org/api/v1/me/repos', { headers }), env });
}

test('add own repos, list them back, withdraw one', async () => {
  const b = bucket(), env = ENV(b);
  let r = await post({ repos: ['ann/one', 'Ann/two', 'ann/one'], agree: true, consent: CONSENT_VERSION }, { env });
  assert.equal(r.status, 200);
  assert.deepEqual((await r.json()).added, ['Ann/one', 'Ann/two']);
  const rec = JSON.parse(b.m.get('req/42/ann--one'));
  assert.equal(rec.gh_id, '42'); assert.equal(rec.login, 'Ann'); assert.equal(rec.consent, CONSENT_VERSION);
  assert.ok(rec.text.includes('I own the repositories')); assert.ok(!('email' in rec));
  r = await post({ withdraw: ['ann/two'] }, { env });
  assert.deepEqual((await r.json()).withdrawn, ['Ann/two']);
  const g = await (await get(env)).json();
  assert.equal(g.login, 'Ann');
  assert.deepEqual(g.requested.map((x) => [x.repo, x.withdrawn]), [['Ann/one', false], ['Ann/two', true]]);
  assert.equal(g.consent.version, CONSENT_VERSION);
});

test('only repositories under the signed-in login', async () => {
  const env = ENV(bucket());
  for (const bad of [['bob/x'], ['ann/x/y'], ['ann/'], ['ann/..'], ['ann/a b'], [], 'ann/x', [7]]) {
    const r = await post({ repos: bad, agree: true, consent: CONSENT_VERSION }, { env });
    assert.equal(r.status, 400, JSON.stringify(bad));
  }
  assert.equal(checkRepos(Array.from({ length: 101 }, (_, i) => `ann/r${i}`), ANN)[1], 'at most 100 repositories per request');
  assert.equal(objectKey(ANN, 'Ann/Repo.js'), 'req/42/ann--repo.js');
});

test('agreement and its current version are required to add', async () => {
  const env = ENV(bucket());
  assert.equal((await post({ repos: ['ann/x'] }, { env })).status, 400);
  assert.equal((await post({ repos: ['ann/x'], agree: true, consent: '2000-01-01' }, { env })).status, 400);
  assert.equal((await post({ repos: ['ann/x'], agree: 'yes', consent: CONSENT_VERSION }, { env })).status, 400);
});

test('no session 401, Hugging Face 403, cross-origin 403, not JSON 415', async () => {
  const env = ENV(bucket()), ok = { repos: ['ann/x'], agree: true, consent: CONSENT_VERSION };
  assert.equal((await post(ok, { env, user: null })).status, 401);
  assert.equal((await get(env, null)).status, 401);
  assert.equal((await post(ok, { env, user: HF })).status, 403);
  assert.equal((await get(env, HF)).status, 403);
  assert.equal((await post(ok, { env, origin: 'https://evil.example' })).status, 403);
  assert.equal((await post(ok, { env, ct: 'text/plain' })).status, 415);
  assert.equal(env.HUB_REPO_REQUESTS.m.size, 0);
});

test('no bucket binding → enabled:false', async () => {
  const env = { ...ENV(null) };
  assert.deepEqual(await (await get(env)).json(), { ok: true, enabled: false });
  assert.deepEqual(await (await post({ repos: ['ann/x'], agree: true, consent: CONSENT_VERSION }, { env })).json(), { ok: true, enabled: false });
});

test('/api/me offers "Add my repos" to GitHub sessions only', async () => {
  const env = ENV(bucket());
  const links = async (u) => (await (await me({ request: new Request('https://hub.xerj.org/api/me', { headers: { cookie: await cookieFor(u) } }), env })).json()).links;
  assert.ok((await links(ANN)).some((l) => l.href === '/add'));
  assert.ok(!(await links(HF)).some((l) => l.href === '/add'));
});
