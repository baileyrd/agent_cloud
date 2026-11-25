// The jobs page (assets/ui.js + assets/jobs.js), run in a stub DOM with node's own test runner (no
// dependencies): `scripts/ci-local.sh web`. A fake gateway answers whoami and the job routes and
// records every request, so each test checks what the page shows and what it sends.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const read = (name) => readFileSync(new URL(`../assets/${name}`, import.meta.url), 'utf8');
const SCRIPTS = [read('ui.js'), read('jobs.js')];
const TOKEN = 'secret-token';
const BASE = '/api/v1/agents/a1/jobs';
const tick = () => new Promise((r) => setTimeout(r, 20));

function element(tag) {
  const e = {
    tag,
    hidden: false,
    textContent: '',
    className: '',
    value: '',
    children: [],
    handlers: {},
    append(...kids) { e.children.push(...kids); },
    replaceChildren(...kids) { e.children = kids; },
    addEventListener(type, fn) { e.handlers[type] = fn; },
  };
  return e;
}

const job = (id, state, more = {}) => ({
  job_id: id, agent: 'a1', state, created_ms: 1, started_ms: null, finished_ms: null, timeout_secs: 3600,
  exit_code: null, idempotency_key: null, reason: null, queued_reason: null, ...more,
});

// Loads the page for agent a1 against a fake gateway. `answers` maps "METHOD path" (query left
// off) to a list of [status, body] replies used in turn (the last repeats); a string body is text,
// and status 0 is a network failure with `body` as its message.
async function load({ scopes, answers = {}, confirm = () => true }) {
  const els = {};
  const sent = [];
  const document = { getElementById: (id) => (els[id] ??= element(id)), createElement: element };
  const reply = (status, body) => ({
    status,
    ok: status < 300,
    json: async () => (typeof body === 'string' ? JSON.parse(body) : body),
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
  });
  async function fetch(url, init) {
    sent.push({ url, method: init.method, body: init.body });
    if (init.headers.Authorization !== `Bearer ${TOKEN}`) return reply(401, { error: 'bad token' });
    if (url === '/api/v1/whoami') return reply(200, { name: 'ops', scopes, agents: ['a1'] });
    const list = answers[`${init.method} ${url.split('?')[0]}`];
    if (!list) return reply(404, { error: 'not found' });
    const [status, body] = list.length > 1 ? list.shift() : list[0];
    if (status === 0) throw new Error(body);
    return reply(status, body);
  }
  const window = { prompt: () => TOKEN, confirm };
  const location = { pathname: '/ui/jobs/a1' };
  const context = { window, document, location, fetch, URLSearchParams, JSON, Date, Number, Promise, String, Object, setTimeout };
  for (const script of SCRIPTS) runInNewContext(script, context);
  await tick();
  const $ = new Proxy(els, { get: (_, id) => document.getElementById(id) });
  return { els: $, sent };
}

const buttons = (els, row) => els.rows.children[row].children[3].children;
const labels = (els) => els.rows.children.map((_, i) => buttons(els, i).map((b) => b.textContent));

test('lists the jobs with cancel only for active ones and only with jobs:write', async () => {
  const jobs = { jobs: [job('j2', 'running'), job('j1', 'succeeded', { exit_code: 0 })] };
  const write = await load({ scopes: ['jobs:read', 'jobs:write'], answers: { [`GET ${BASE}`]: [[200, jobs]] } });
  assert.deepEqual(labels(write.els), [['Show', 'Cancel'], ['Show']]);
  assert.equal(write.els.rows.children[1].children[1].textContent, 'succeeded · exit 0');
  assert.equal(write.els.submit.hidden, false);
  const read = await load({ scopes: ['jobs:read'], answers: { [`GET ${BASE}`]: [[200, jobs]] } });
  assert.deepEqual(labels(read.els), [['Show'], ['Show']]);
  assert.equal(read.els.submit.hidden, true);
});

test('submitting follows the job with long-polls, then shows output and witness as text', async () => {
  // ADR-029's line form, as `ac_domain::witness::render` writes it: not JSON.
  const record = 'agent a1\njob j9\nstate succeeded\nchanged /workspace/a.txt\n';
  const { els, sent } = await load({
    scopes: ['jobs:read', 'jobs:write'],
    answers: {
      [`GET ${BASE}`]: [[200, { jobs: [] }]],
      [`POST ${BASE}`]: [[202, job('j9', 'queued')]],
      [`GET ${BASE}/j9`]: [[200, job('j9', 'queued', { queued_reason: 'slots' })], [200, job('j9', 'succeeded', { exit_code: 0 })]],
      [`GET ${BASE}/j9/output`]: [[200, '<img src=x onerror=alert(1)> done']],
      [`GET ${BASE}/j9/witness`]: [[200, { record, sha256: 'ab12' }]],
    },
  });
  sent.length = 0;
  els.prompt.value = 'list the files';
  els.timeout.value = '60';
  await els.submit.handlers.submit({ preventDefault() {} });
  // The first poll answered at once (queued), so the next waits out the 1 s floor.
  await new Promise((r) => setTimeout(r, 1100));
  assert.deepEqual(JSON.parse(sent[0].body), { prompt: 'list the files', timeout_secs: 60 });
  const polls = sent.filter((r) => r.url.startsWith(`${BASE}/j9?`) || r.url === `${BASE}/j9`).map((r) => r.url);
  assert.deepEqual(polls, [`${BASE}/j9`, `${BASE}/j9?wait=25`]);
  assert.equal(els.state.textContent, 'succeeded · exit 0');
  assert.equal(els.output.textContent, '<img src=x onerror=alert(1)> done');
  assert.equal(els.witness.textContent, `${record}sha256 ab12`, 'the record verbatim, then its digest');
});

test('cancel is confirmed, and the gateway refusal is shown', async () => {
  let answer = false;
  const { els, sent } = await load({
    scopes: ['jobs:write'],
    confirm: () => answer,
    answers: {
      [`GET ${BASE}`]: [[200, { jobs: [job('j2', 'running')] }]],
      [`POST ${BASE}/j2/cancel`]: [[403, { error: 'needs jobs:write for a1' }]],
    },
  });
  sent.length = 0;
  await buttons(els, 0)[1].onclick();
  assert.deepEqual(sent, []);
  answer = true;
  await buttons(els, 0)[1].onclick();
  assert.deepEqual([sent[0].method, sent[0].url], ['POST', `${BASE}/j2/cancel`]);
  assert.equal(els.note.textContent, 'Cancel j2: needs jobs:write for a1');
});

test('picking another job stops following the first', async () => {
  const { els, sent } = await load({
    scopes: ['jobs:read'],
    answers: {
      [`GET ${BASE}`]: [[200, { jobs: [job('j2', 'running'), job('j1', 'failed', { reason: 'exit 1' })] }]],
      [`GET ${BASE}/j2`]: [[200, job('j2', 'running')]],
      [`GET ${BASE}/j1`]: [[200, job('j1', 'failed', { reason: 'exit 1' })]],
      [`GET ${BASE}/j1/output`]: [[409, { error: 'no output' }]],
      [`GET ${BASE}/j1/witness`]: [[200, { record: 'job j1\n', sha256: 'cd' }]],
    },
  });
  buttons(els, 0)[0].onclick();
  await tick();
  await buttons(els, 1)[0].onclick();
  await tick();
  const before = sent.filter((r) => r.url.startsWith(`${BASE}/j2`)).length;
  await tick();
  assert.equal(sent.filter((r) => r.url.startsWith(`${BASE}/j2`)).length, before, 'j2 is no longer polled');
  assert.equal(els.picked.textContent, 'j1');
  assert.equal(els.state.textContent, 'failed · exit 1');
  assert.equal(els.output.textContent, '(no output: no output)');
});

test('a failure while following a picked job is shown, not left unhandled', async () => {
  const { els } = await load({
    scopes: ['jobs:read'],
    answers: {
      [`GET ${BASE}`]: [[200, { jobs: [job('j1', 'succeeded', { exit_code: 0 })] }]],
      [`GET ${BASE}/j1`]: [[200, job('j1', 'succeeded', { exit_code: 0 })]],
      [`GET ${BASE}/j1/output`]: [[200, 'done']],
      [`GET ${BASE}/j1/witness`]: [[0, 'network down']],
    },
  });
  buttons(els, 0)[0].onclick();
  await tick();
  await tick();
  assert.equal(els.note.textContent, 'j1: network down');
  assert.equal(els.note.className, 'bad');
});
