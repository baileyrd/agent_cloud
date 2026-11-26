// The admin page (assets/ui.js + assets/admin.js), run in a stub DOM with node's own test runner
// (no dependencies): `scripts/ci-local.sh web`. A fake gateway answers whoami, the token and backup
// routes and the agent list, and records every request.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const read = (name) => readFileSync(new URL(`../assets/${name}`, import.meta.url), 'utf8');
const SCRIPTS = [read('ui.js'), read('admin.js')];
const TOKEN = 'secret-token';
const SECRET = 'a'.repeat(64);
const tick = () => new Promise((r) => setTimeout(r, 20));

function element(tag) {
  const e = {
    tag,
    hidden: false,
    textContent: '',
    className: '',
    value: '',
    checked: false,
    children: [],
    handlers: {},
    append(...kids) { e.children.push(...kids); },
    replaceChildren(...kids) { e.children = kids; },
    addEventListener(type, fn) { e.handlers[type] = fn; },
  };
  return e;
}

const TOKENS = {
  tokens: [
    { name: 'owner', scopes: ['admin'], agents: '*', expires: null, created: 1764115200, live: true },
    { name: 'ci', scopes: ['jobs:read', 'jobs:write'], agents: ['a1', 'b2'], expires: 1764201600, created: 1764115200, live: false },
  ],
};
const BACKUPS = {
  dest: '/mnt/agent-backups',
  run: { active: 'inactive', result: 'success', started: 1764127800, finished: 1764127860 },
  state: ['20261125-033000', '20261126-033000'],
  agents: [
    { id: 'a1', backups: [
      { id: '20261125-033000', parent: null, bytes: 1288490188, restorable: true },
      { id: '20261126-033000', parent: '20261125-033000', bytes: 3250585, restorable: true },
    ] },
    { id: 'b2', backups: [{ id: '20261126-033000', parent: '20261120-033000', bytes: 10, restorable: false }] },
  ],
};
const AGENTS = { agents: [{ id: 'a1', state: 'paused' }, { id: 'b2', state: 'paused' }] };
const ROUTES = {
  'GET /api/v1/tokens': [[200, TOKENS]],
  'GET /api/v1/backups': [[200, BACKUPS]],
  'GET /api/v1/agents': [[200, AGENTS]],
};

async function load({ agents = '*', scopes = ['admin'], answers = {}, confirm = () => true, clipboard } = {}) {
  const els = {};
  const sent = [];
  const prompts = [TOKEN];
  const document = {
    getElementById: (id) => (els[id] ??= element(id)),
    createElement: element,
  };
  const reply = (status, body) => ({ status, ok: status < 300, json: async () => body });
  const routes = { ...ROUTES, ...answers };
  async function fetch(url, init) {
    sent.push({ url, method: init.method, body: init.body });
    if (url === '/api/v1/whoami') return reply(200, { name: 'ops', scopes, agents });
    const list = routes[`${init.method} ${url}`];
    if (!list) return reply(404, { error: 'not found' });
    const [status, body] = list.length > 1 ? list.shift() : list[0];
    return reply(status, body);
  }
  const navigator = { clipboard: clipboard ?? { writeText: async () => {} } };
  const window = { prompt: () => prompts.shift() ?? null, confirm, navigator };
  const context = { window, document, fetch, URLSearchParams, Promise, String, Object, Date, Set, Array };
  for (const script of SCRIPTS) runInNewContext(script, context);
  await tick();
  // Any element looked up by id, also one the page never touched; the box for each scope by name.
  const get = (id) => document.getElementById(id);
  return { els: new Proxy(els, { get: (_, id) => get(id) }), sent };
}

const text = (td) => td.textContent || td.children.map((c) => c.textContent).join('');
const rows = (body) => body.children.map((tr) => tr.children.map(text));
const submit = (els) => els.create.handlers.submit({ preventDefault() {} });

test('a token without admin for every agent sees nothing and sends nothing more', async () => {
  for (const who of [{ scopes: ['agents:read'] }, { agents: ['a1'] }]) {
    const { els, sent } = await load(who);
    assert.equal(els.scopes.children.length, 0, 'the page was not set up');
    assert.match(els.note.textContent, /admin for every agent/);
    assert.deepEqual(sent.map((r) => r.url), ['/api/v1/whoami']);
  }
});

test('lists tokens, never a digest, with expiry and revoke', async () => {
  const { els } = await load();
  assert.equal(els.admin.hidden, false);
  assert.deepEqual(rows(els.tokens), [
    ['owner', 'admin', '*', '2025-11-26 00:00 UTC', 'never', 'Revoke'],
    ['ci', 'jobs:read, jobs:write', 'a1, b2', '2025-11-26 00:00 UTC', 'expired 2025-11-27 00:00 UTC', 'Revoke'],
  ]);
  assert.equal(els.scopes.children.length, 8);
});

test('create sends the grant, shows the secret once, and the next action drops it', async () => {
  const { els, sent } = await load({
    answers: { 'POST /api/v1/tokens/ci2': [[201, { name: 'ci2', token: SECRET, expires: 1766707200 }]] },
  });
  els['new-name'].value = 'ci2';
  els['new-agents'].value = 'a1, b2';
  els['new-expires'].value = '30d';
  els['scope-jobs:read'].checked = true;
  els['scope-jobs:write'].checked = true;
  sent.length = 0;
  await submit(els);
  assert.deepEqual([sent[0].method, sent[0].url], ['POST', '/api/v1/tokens/ci2']);
  assert.deepEqual(JSON.parse(sent[0].body), { scopes: ['jobs:read', 'jobs:write'], agents: ['a1', 'b2'], expires: '30d' });
  assert.equal(els.secret.hidden, false);
  assert.equal(els['secret-text'].textContent, SECRET);
  await els.refresh.handlers.click();
  assert.equal(els.secret.hidden, true);
  assert.equal(els['secret-text'].textContent, '', 'gone from the page');
});

test('create without a lifetime sends none, with * sends "*", and without scopes sends nothing', async () => {
  const { els, sent } = await load({ answers: { 'POST /api/v1/tokens/ops2': [[201, { name: 'ops2', token: SECRET }]] } });
  els['new-name'].value = 'ops2';
  els['new-agents'].value = '*';
  sent.length = 0;
  await submit(els);
  assert.deepEqual(sent, []);
  assert.match(els.note.textContent, /at least one scope/);
  els['scope-admin'].checked = true;
  els['new-expires'].value = '';
  await submit(els);
  assert.deepEqual(JSON.parse(sent[0].body), { scopes: ['admin'], agents: '*' });
});

test('revoke is confirmed, and the lockout refusal is shown', async () => {
  let answer = false;
  const { els, sent } = await load({
    confirm: () => answer,
    answers: { 'DELETE /api/v1/tokens/owner': [[409, { error: 'refused: no live token with admin for * would remain' }]] },
  });
  sent.length = 0;
  await els.tokens.children[0].children[5].children[0].onclick();
  assert.deepEqual(sent, []);
  answer = true;
  await els.tokens.children[0].children[5].children[0].onclick();
  assert.deepEqual([sent[0].method, sent[0].url], ['DELETE', '/api/v1/tokens/owner']);
  assert.match(els.note.textContent, /no live token with admin/);
  assert.equal(els.note.className, 'bad');
});

test('copy writes the secret to the clipboard', async () => {
  const copied = [];
  const { els } = await load({
    clipboard: { writeText: async (t) => { copied.push(t); } },
    answers: { 'POST /api/v1/tokens/x': [[201, { name: 'x', token: SECRET }]] },
  });
  els['new-name'].value = 'x';
  els['new-agents'].value = '*';
  els['scope-admin'].checked = true;
  await submit(els);
  await els.copy.handlers.click();
  assert.deepEqual(copied, [SECRET]);
});

test('backups: the last run, state copies, streams, and restore for paused agents', async () => {
  const { els } = await load();
  assert.equal(els.backups.hidden, false);
  assert.equal(els.run.textContent,
    'Last run: success, started 2025-11-26 03:30 UTC, finished 2025-11-26 03:31 UTC.');
  assert.equal(els.state.textContent, 'State copies: 2, newest 20261126-033000.');
  assert.deepEqual(rows(els.streams), [
    ['a1', '20261125-033000', 'full', '1.2 GiB', 'Restore'],
    ['a1', '20261126-033000', 'incremental from 20261125-033000', '3.1 MiB', 'Restore'],
    ['b2', '20261126-033000', 'incremental from 20261120-033000', '10 B', 'chain broken'],
  ]);
});

test('no restore for a running agent', async () => {
  const { els } = await load({ answers: { 'GET /api/v1/agents': [[200, { agents: [{ id: 'a1', state: 'running' }] }]] } });
  assert.deepEqual(rows(els.streams).map((r) => r[4]), ['', '', 'chain broken']);
});

test('backups off shows why', async () => {
  const { els } = await load({ answers: { 'GET /api/v1/backups': [[404, { error: 'no backup destination (set AC_BACKUP_DEST)' }]] } });
  assert.equal(els.backups.hidden, true);
  assert.equal(els['backups-off'].hidden, false);
  assert.match(els['backups-off'].textContent, /AC_BACKUP_DEST/);
});

test('back up now starts the unit; a running one is refused', async () => {
  const { els, sent } = await load({
    answers: { 'POST /api/v1/backups': [[202, { started: 'ac-backup.service' }], [409, { error: 'a backup is already running' }]] },
  });
  sent.length = 0;
  await els['back-up'].handlers.click();
  assert.deepEqual([sent[0].method, sent[0].url], ['POST', '/api/v1/backups']);
  assert.match(els.note.textContent, /Backup started/);
  await els['back-up'].handlers.click();
  assert.match(els.note.textContent, /already running/);
});

test('restore is confirmed and names the backup', async () => {
  const { els, sent } = await load({
    answers: { 'POST /api/v1/agents/a1/restore-backup': [[200, { id: 'a1', restored: '20261126-033000', kept: '20261126-120000' }]] },
  });
  sent.length = 0;
  await els.streams.children[1].children[4].children[0].onclick();
  assert.deepEqual([sent[0].method, sent[0].url], ['POST', '/api/v1/agents/a1/restore-backup']);
  assert.deepEqual(JSON.parse(sent[0].body), { backup: '20261126-033000' });
  assert.equal(els.note.textContent, 'Restored a1 from 20261126-033000. The old workspace is checkpoint 20261126-120000.');
});

// A created secret, shown; `els` and `sent` of a page that has one.
async function withSecret(opts = {}) {
  const page = await load({
    ...opts,
    answers: { 'POST /api/v1/tokens/x': [[201, { name: 'x', token: SECRET }]], ...opts.answers },
  });
  page.els['new-name'].value = 'x';
  page.els['new-agents'].value = '*';
  page.els['scope-admin'].checked = true;
  await submit(page.els);
  assert.equal(page.els['secret-text'].textContent, SECRET);
  return page;
}

test('cancelling revoke or restore keeps a new secret; going ahead drops it', async () => {
  let answer = false;
  const { els, sent } = await withSecret({ confirm: () => answer });
  sent.length = 0;
  await els.tokens.children[0].children[5].children[0].onclick();
  await els.streams.children[0].children[4].children[0].onclick();
  assert.deepEqual(sent, []);
  assert.equal(els.secret.hidden, false);
  assert.equal(els['secret-text'].textContent, SECRET, 'still there to copy');
  answer = true;
  await els.tokens.children[0].children[5].children[0].onclick();
  assert.equal(els['secret-text'].textContent, '');
});

test('revoking the signed-in token says so in the confirm', async () => {
  const asked = [];
  const { els } = await load({
    confirm: (q) => { asked.push(q); return false; },
    answers: { 'GET /api/v1/tokens': [[200, { tokens: [{ ...TOKENS.tokens[0], name: 'ops' }, TOKENS.tokens[1]] }]] },
  });
  await els.tokens.children[0].children[5].children[0].onclick();
  await els.tokens.children[1].children[5].children[0].onclick();
  assert.match(asked[0], /this page signed in with/);
  assert.doesNotMatch(asked[1], /this page signed in with/);
});

test('a refused create or restore is shown and sends nothing more', async () => {
  const { els } = await load({
    answers: {
      'POST /api/v1/tokens/x': [[403, { error: 'token ops expires at 1764201600: a token it creates must expire no later' }]],
      'POST /api/v1/agents/a1/restore-backup': [[409, { error: 'agent a1 has a running VM' }]],
    },
  });
  els['new-name'].value = 'x';
  els['new-agents'].value = '*';
  els['scope-admin'].checked = true;
  await submit(els);
  assert.match(els.note.textContent, /must expire no later/);
  assert.equal(els.secret.hidden, true);
  await els.streams.children[0].children[4].children[0].onclick();
  assert.match(els.note.textContent, /^Restore a1: .*running VM/);
  assert.equal(els.note.className, 'bad');
});

test('a failed token list clears the table; a failed agent list offers no restore and says why', async () => {
  const { els } = await load({
    answers: {
      'GET /api/v1/tokens': [[200, TOKENS], [500, { error: 'tokens file unreadable' }]],
      'GET /api/v1/agents': [[200, AGENTS], [503, { error: 'store down' }]],
    },
  });
  assert.equal(els.tokens.children.length, 2);
  assert.equal(els.refresh.hidden, false);
  await els.refresh.handlers.click();
  assert.equal(els.tokens.children.length, 0);
  assert.match(els.note.textContent, /^Agents: .*no restore/);
  assert.deepEqual(rows(els.streams).map((r) => r[4]), ['', '', 'chain broken']);
});
