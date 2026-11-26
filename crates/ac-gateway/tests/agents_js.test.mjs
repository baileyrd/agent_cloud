// The agents page (assets/ui.js + assets/agents.js), run in a stub DOM with node's own test runner
// (no dependencies): `scripts/ci-local.sh web`. A fake gateway answers whoami and the agents routes
// and records every request, so each test checks what the page shows and what it sends.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const read = (name) => readFileSync(new URL(`../assets/${name}`, import.meta.url), 'utf8');
const SCRIPTS = [read('ui.js'), read('agents.js')];
const TOKEN = 'secret-token';
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

// Loads the page against a fake gateway. `answers` maps "METHOD path" (query left off) to a list
// of [status, body] replies, used in turn; the last one repeats.
async function load({ scopes, tokens = [TOKEN], answers = {}, confirm = () => true }) {
  const els = {};
  const sent = [];
  const prompts = [...tokens];
  const document = {
    getElementById: (id) => (els[id] ??= element(id)),
    createElement: element,
  };
  const reply = (status, body) => ({ status, ok: status < 300, json: async () => body });
  async function fetch(url, init) {
    sent.push({ url, method: init.method, auth: init.headers.Authorization, body: init.body });
    if (init.headers.Authorization !== `Bearer ${TOKEN}`) return reply(401, { error: 'bad token' });
    if (url === '/api/v1/whoami') return reply(200, { name: 'ops', scopes, agents: '*' });
    const list = answers[`${init.method} ${url.split('?')[0]}`];
    if (!list) return reply(404, { error: 'not found' });
    const [status, body] = list.length > 1 ? list.shift() : list[0];
    return reply(status, body);
  }
  const window = { prompt: () => prompts.shift() ?? null, confirm };
  const context = { window, document, fetch, URLSearchParams, setInterval() {}, Promise, String, Object };
  for (const script of SCRIPTS) runInNewContext(script, context);
  await tick();
  return { els: new Proxy(els, { get: (all, id) => document.getElementById(id) }), sent };
}

const AGENTS = {
  agents: [
    { id: 'a1', state: 'running' },
    { id: 'b2', state: 'paused', health: 'unhealthy' },
    { id: 'c3', state: 'destroyed' },
  ],
};
const text = (td) => td.textContent || td.children.map((c) => c.textContent).join('');
const cells = (els) => els.rows.children.map((tr) => tr.children.map(text));
const buttons = (els, row) => els.rows.children[row].children[3].children;
const labels = (els) => els.rows.children.map((_, i) => buttons(els, i).map((b) => b.textContent));

test('lists the agents with the actions each state allows', async () => {
  const { els } = await load({ scopes: ['agents:read', 'agents:write'], answers: { 'GET /api/v1/agents': [[200, AGENTS]] } });
  assert.deepEqual(cells(els).map((c) => c.slice(0, 3)), [
    ['a1', 'running', ''],
    ['b2', 'paused', 'unhealthy'],
    ['c3', 'destroyed', ''],
  ]);
  assert.deepEqual(labels(els), [['Pause', 'Destroy', 'Purge'], ['Resume', 'Destroy', 'Purge'], ['Purge']]);
  assert.equal(els.create.hidden, false);
  assert.equal(els.empty.hidden, true);
  assert.equal(els.rows.children[0].children[0].children[0].href, '/ui/agent/a1');
});

test('the capacity line shows room, /dev/shm and job slots', async () => {
  const figures = {
    shm_total_mib: 9000, shm_free_mib: 4600, guest_mib: 2048, running_agents: 2, committed_mib: 4096,
    margin_mib: 512, max_more_agents: 1, running_jobs: 1, queued_jobs: 3, max_running_jobs: 2,
  };
  const { els } = await load({
    scopes: ['agents:read'],
    answers: { 'GET /api/v1/agents': [[200, AGENTS]], 'GET /api/v1/capacity': [[200, figures]] },
  });
  assert.equal(els.capacity.hidden, false);
  assert.equal(els.capacity.textContent,
    'room for 1 more agent · 2 running · /dev/shm 4600 of 9000 MiB free · 2048 MiB per guest · jobs: 1 of 2 running, 3 queued');
});

test('a read-only token sees the list without any controls', async () => {
  const { els, sent } = await load({ scopes: ['agents:read'], answers: { 'GET /api/v1/agents': [[200, AGENTS]] } });
  assert.deepEqual(labels(els), [[], [], []]);
  assert.equal(els.create.hidden, true);
  assert.deepEqual(sent.map((r) => [r.method, r.url]), [
    ['GET', '/api/v1/whoami'],
    ['GET', '/api/v1/agents'],
    ['GET', '/api/v1/capacity'],
  ]);
  assert.equal(els.capacity.hidden, true, 'no capacity line when the gateway refuses it');
});

test('a wrong token is asked for again, and the token never leaves the header', async () => {
  const { els, sent } = await load({
    scopes: ['agents:read'],
    tokens: ['wrong', TOKEN],
    answers: { 'GET /api/v1/agents': [[200, { agents: [] }]] },
  });
  assert.deepEqual(sent.map((r) => r.url), ['/api/v1/whoami', '/api/v1/whoami', '/api/v1/agents', '/api/v1/capacity']);
  assert.equal(els.empty.hidden, false);
  for (const r of sent) assert.ok(!r.url.includes(TOKEN) && !(r.body ?? '').includes(TOKEN));
});

test('no token leaves the page signed out and sends nothing', async () => {
  const { els, sent } = await load({ scopes: [], tokens: [] });
  assert.deepEqual(sent, []);
  assert.match(els.note.textContent, /No token/);
});

test('pausing an agent with a job asks, then retries with force', async () => {
  const asked = [];
  const { els, sent } = await load({
    scopes: ['agents:write'],
    confirm: (text) => (asked.push(text), true),
    answers: {
      'GET /api/v1/agents': [[200, AGENTS]],
      'POST /api/v1/agents/a1/pause': [[409, { error: 'agent a1 has a queued or running job' }], [200, {}]],
    },
  });
  sent.length = 0;
  await buttons(els, 0)[0].onclick();
  await tick();
  assert.deepEqual(sent.slice(0, 2).map((r) => [r.method, r.url]), [
    ['POST', '/api/v1/agents/a1/pause'],
    ['POST', '/api/v1/agents/a1/pause?force=true'],
  ]);
  assert.match(asked[0], /has a queued or running job/);
  assert.equal(els.note.textContent, 'Pause a1: done.');
});

test('purge is confirmed first, and declining sends nothing', async () => {
  let answer = false;
  const { els, sent } = await load({
    scopes: ['agents:write'],
    confirm: () => answer,
    answers: { 'GET /api/v1/agents': [[200, AGENTS]], 'DELETE /api/v1/agents/c3': [[200, {}]] },
  });
  sent.length = 0;
  await buttons(els, 2)[0].onclick();
  assert.deepEqual(sent, []);
  answer = true;
  await buttons(els, 2)[0].onclick();
  assert.deepEqual([sent[0].method, sent[0].url], ['DELETE', '/api/v1/agents/c3?purge=true']);
});

test('create sends the id URL-encoded and shows the gateway refusal', async () => {
  const { els, sent } = await load({
    scopes: ['agents:write'],
    answers: { 'GET /api/v1/agents': [[200, { agents: [] }]], 'POST /api/v1/agents/a%2Fb': [[400, { error: 'invalid agent id' }]] },
  });
  sent.length = 0;
  els['new-id'].value = ' a/b ';
  await els.create.handlers.submit({ preventDefault() {} });
  assert.deepEqual([sent[0].method, sent[0].url], ['POST', '/api/v1/agents/a%2Fb']);
  assert.equal(els.note.textContent, 'Create a/b: invalid agent id');
  assert.equal(els.note.className, 'bad');
});
