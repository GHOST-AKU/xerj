// /api/v1/me/repos — a signed-in GitHub owner adds their own repositories to the Corpus Hub (the /add page).
//
//   GET  → 200 { ok, enabled, login, consent: {version, text},
//                requested: [{repo, at, withdrawn}],   what this account asked for here
//                on_hub: ["owner/name", ...] }          repos already credited to this login on the hub
//   POST { repos: ["login/name", ...], agree: true, consent: "<version>" }      add (max 100 per request)
//   POST { withdraw: ["login/name", ...] }                                      take back
//        → 200 { ok, added|withdrawn: [...] }
//   401 without a session · 403 a Hugging Face session, a repo outside the account, cross-origin
//   400 bad names / no agreement / stale consent version · 415 not JSON · 413 too large
//   No HUB_REPO_REQUESTS binding → GET/POST 200 { ok:true, enabled:false }.
//
// Each request is one R2 object, req/<github user id>/<owner--name>, holding the repo, the login and
// GitHub user id from the signed session, the time, and the consent text version agreed to. Nothing
// else: no email, no IP. A withdraw rewrites the object with withdrawn:true. The outreach pipeline
// (outreach/hub_web_consent.py) reads the bucket, checks on GitHub that the repository's owner id is
// the signed-in account's id (a renamed login cannot claim someone else's repos), and feeds the
// consents into the same ingest → publish → checked merge as email replies.
//
// Only repositories under the account's own login: org repositories need an org admin, which the
// public-profile sign-in cannot tell (GitHub scope none).
import { json, requireUser } from '../../../_lib/session.js';
import { details } from '../../../_lib/details.js';

export const CONSENT_VERSION = '2026-10-27';
export const CONSENT_TEXT =
  'I own the repositories I selected, and I agree that XERJ may include them in the public XERJ Corpus Hub ' +
  '(hub.xerj.org), an open index that AI coding agents search, with attribution to me and to each repository. ' +
  'XERJ uses the hub for retrieval and does not train models on it. I can withdraw a repository at any time on this page.';

const MAX_BODY = 16 * 1024;
const MAX_REPOS = 100;          // per request
const MAX_LIST = 1000;          // requests listed back
const NAME_RE = /^[A-Za-z0-9._-]{1,100}$/;
const ORIGIN_OK = /^https:\/\/((www\.)?hub\.xerj\.org|([a-z0-9-]+\.)?xerj-hub\.pages\.dev)$|^http:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/;

export const objectKey = (user, repo) => `req/${user.id}/${repo.toLowerCase().replace('/', '--')}`;

async function signedInOwner(request, env) {
  const [user, deny] = await requireUser(request, env);
  if (deny) return [null, deny];
  if (user.provider !== 'github') {
    return [null, json({ ok: false, enabled: true, error: 'sign in with GitHub to add your repositories' }, 403)];
  }
  return [user, null];
}

async function listRequests(bucket, user) {
  const prefix = `req/${user.id}/`, out = [];
  let cursor;
  for (let page = 0; page < 10 && out.length < MAX_LIST; page++) {
    const res = await bucket.list({ prefix, cursor });
    out.push(...res.objects);
    if (!res.truncated) break;
    cursor = res.cursor;
  }
  const rows = await Promise.all(out.slice(0, MAX_LIST).map(async (o) => {
    const b = await bucket.get(o.key).catch(() => null);
    try { const r = b ? JSON.parse(await b.text()) : null; return r && { repo: r.repo, at: r.at, withdrawn: !!r.withdrawn }; } catch (_) { return null; }
  }));
  return rows.filter(Boolean).sort((a, b) => (a.repo.toLowerCase() < b.repo.toLowerCase() ? -1 : 1));
}

function onHub(user, data = details()) {
  const c = data && data.contributors && data.contributors[user.login.toLowerCase()];
  return c ? [...new Set((c.work || []).map((w) => w.repo).filter(Boolean))].sort() : [];
}

export async function onRequestGet({ request, env }) {
  const [user, deny] = await signedInOwner(request, env);
  if (deny) return deny;
  const bucket = env.HUB_REPO_REQUESTS;
  if (!bucket) return json({ ok: true, enabled: false });
  let requested;
  try { requested = await listRequests(bucket, user); } catch (e) {
    console.log('[me/repos] R2 list failed:', String(e));
    return json({ ok: false, enabled: true, error: 'storage unavailable' }, 503);
  }
  return json({ ok: true, enabled: true, login: user.login, consent: { version: CONSENT_VERSION, text: CONSENT_TEXT },
                requested, on_hub: onHub(user) });
}

/** "login/name" strings → [owner/name...] or an error message. */
export function checkRepos(list, user) {
  if (!Array.isArray(list) || !list.length) return [null, 'no repositories given'];
  if (list.length > MAX_REPOS) return [null, `at most ${MAX_REPOS} repositories per request`];
  const out = new Set();
  for (const r of list) {
    const [owner, name, extra] = typeof r === 'string' ? r.split('/') : [];
    if (extra !== undefined || !owner || !NAME_RE.test(name || '') || name === '.' || name === '..') return [null, `not a repository name: ${String(r).slice(0, 120)}`];
    if (owner.toLowerCase() !== user.login.toLowerCase()) return [null, `only repositories under ${user.login} can be added from this account: ${r}`];
    out.add(`${user.login}/${name}`);
  }
  return [[...out], null];
}

export async function onRequestPost({ request, env }) {
  const origin = request.headers.get('origin');
  if (origin && !ORIGIN_OK.test(origin)) return json({ ok: false, error: 'cross-origin requests are not accepted' }, 403);
  const [user, deny] = await signedInOwner(request, env);
  if (deny) return deny;
  const bucket = env.HUB_REPO_REQUESTS;
  if (!bucket) return json({ ok: true, enabled: false });
  const ct = (request.headers.get('content-type') || '').toLowerCase();
  if (!ct.includes('application/json')) return json({ ok: false, error: 'content-type must be application/json' }, 415);
  let raw;
  try { raw = await request.text(); } catch (_) { return json({ ok: false, error: 'could not read body' }, 400); }
  if (raw.length > MAX_BODY) return json({ ok: false, error: 'body too large' }, 413);
  let body;
  try { body = JSON.parse(raw); } catch (_) { return json({ ok: false, error: 'invalid JSON body' }, 400); }
  if (!body || typeof body !== 'object') return json({ ok: false, error: 'invalid JSON body' }, 400);

  const withdraw = body.withdraw !== undefined;
  const [repos, bad] = checkRepos(withdraw ? body.withdraw : body.repos, user);
  if (bad) return json({ ok: false, error: bad }, 400);
  if (!withdraw && body.agree !== true) return json({ ok: false, error: 'tick the agreement to add repositories' }, 400);
  if (!withdraw && body.consent !== CONSENT_VERSION) return json({ ok: false, error: 'the agreement text changed; reload the page' }, 400);

  const at = new Date().toISOString();
  try {
    await Promise.all(repos.map((repo) => bucket.put(objectKey(user, repo), JSON.stringify({
      repo, login: user.login, gh_id: user.id, name: user.name, at,
      ...(withdraw ? { withdrawn: true } : { consent: CONSENT_VERSION, text: CONSENT_TEXT }),
    }), { httpMetadata: { contentType: 'application/json' } })));
  } catch (e) {
    console.log('[me/repos] R2 put failed:', String(e));
    return json({ ok: false, error: 'storage unavailable' }, 503);
  }
  return json({ ok: true, [withdraw ? 'withdrawn' : 'added']: repos });
}
