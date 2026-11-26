// One agent's page (assets/ui.js + assets/agent.js): checkpoints, restore and fork, run in a stub
// DOM with node's own test runner (no dependencies): `scripts/ci-local.sh web`. A fake gateway
// answers whoami and the agent routes and records every request.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const read = (name) => readFileSync(new URL(`../assets/${name}`, import.meta.url), 'utf8');
const SCRIPTS = [read('ui.js'), read('agent.js')];
const TOKEN = 'secret-token';
const A = '/api/v1/agents/a1';
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

// Loads the page for agent a1. `answers` maps "METHOD path" to [status, body] replies used in
// turn (the last repeats).
async function load({ scopes, state = 'running', answers = {}, confirm = () => true }) {
  const els = {};
  const sent = [];
  const document = { getElementById: (id) => (els[id] ??= element(id)), createElement: element };
  const reply = (status, body) => ({ status, ok: status < 300, json: async () => body });
  const all = {
    [`GET ${A}`]: [[200, { id: 'a1', state }]],
    [`GET ${A}/checkpoints`]: [[200, { id: 'a1', max: 5, checkpoints: [{ id: 'c1', label: 'before upgrade' }, { id: 'c2' }] }]],
    ...answers,
  };
  async function fetch(url, init) {
    sent.push({ url, method: init.method, body: init.body && JSON.parse(init.body) });
    if (init.headers.Authorization !== `Bearer ${TOKEN}`) return reply(401, { error: 'bad token' });
    if (url === '/api/v1/whoami') return reply(200, { name: 'ops', scopes, agents: ['a1'] });
    const list = all[`${init.method} ${url.split('?')[0]}`];
    if (!list) return reply(404, { error: 'not found' });
    const [status, body] = list.length > 1 ? list.shift() : list[0];
    return reply(status, body);
  }
  const window = { prompt: () => TOKEN, confirm };
  const location = { pathname: '/ui/agent/a1' };
  for (const script of SCRIPTS) runInNewContext(script, { window, document, location, fetch, URLSearchParams, Promise, String, Object });
  await tick();
  const $ = new Proxy(els, { get: (_, id) => document.getElementById(id) });
  return { els: $, sent };
}

const labels = (els) => els.rows.children.map((tr) => tr.children[2].children.map((b) => b.textContent));

test('lists checkpoints; restore is offered only on a paused agent, and only with agents:write', async () => {
  const running = await load({ scopes: ['agents:write'] });
  assert.deepEqual(running.els.rows.children.map((tr) => [tr.children[0].textContent, tr.children[1].textContent]),
    [['c1', 'before upgrade'], ['c2', '']]);
  assert.equal(running.els.count.textContent, '2 of 5');
  assert.deepEqual(labels(running.els), [['Delete'], ['Delete']]);
  assert.equal(running.els['restore-hint'].hidden, false);
  assert.equal(running.els.jobs.href, '/ui/jobs/a1');
  const paused = await load({ scopes: ['agents:write'], state: 'paused' });
  assert.deepEqual(labels(paused.els), [['Restore', 'Delete'], ['Restore', 'Delete']]);
  assert.equal(paused.els['restore-hint'].hidden, true);
  const reader = await load({ scopes: ['agents:read'], state: 'paused' });
  assert.deepEqual(labels(reader.els), [[], []]);
  assert.equal(reader.els.checkpoint.hidden, true);
  assert.equal(reader.els.fork.hidden, true);
});

test('restore is confirmed and sends the checkpoint id', async () => {
  let answer = false;
  const { els, sent } = await load({
    scopes: ['agents:write'],
    state: 'paused',
    confirm: () => answer,
    answers: { [`POST ${A}/restore`]: [[200, { id: 'a1', restored: 'c1', kept: 'c3' }]] },
  });
  sent.length = 0;
  await els.rows.children[0].children[2].children[0].onclick();
  assert.deepEqual(sent, []);
  answer = true;
  await els.rows.children[0].children[2].children[0].onclick();
  assert.deepEqual([sent[0].method, sent[0].url, sent[0].body], ['POST', `${A}/restore`, { checkpoint: 'c1' }]);
  assert.equal(els.note.textContent, 'Restore c1: done.');
});

test('a checkpoint takes an optional label; the gateway refusal at the cap is shown', async () => {
  const { els, sent } = await load({
    scopes: ['agents:write'],
    answers: { [`POST ${A}/checkpoints`]: [[201, { id: 'c3' }], [409, { error: 'a1 has 5 checkpoints, the most it may keep' }]] },
  });
  sent.length = 0;
  els.label.value = ' nightly ';
  await els.checkpoint.handlers.submit({ preventDefault() {} });
  assert.deepEqual(sent[0].body, { label: 'nightly' });
  assert.equal(els.label.value, '');
  await els.checkpoint.handlers.submit({ preventDefault() {} });
  assert.deepEqual(sent.filter((r) => r.method === 'POST')[1].body, {});
  assert.equal(els.note.textContent, 'Checkpoint: a1 has 5 checkpoints, the most it may keep');
  assert.equal(els.note.className, 'bad');
});

test('fork sends the new id, and a checkpoint only when one is picked', async () => {
  const { els, sent } = await load({
    scopes: ['agents:write'],
    answers: { [`POST ${A}/fork`]: [[201, { id: 'b2', state: 'running' }]] },
  });
  assert.deepEqual(els.from.children.map((o) => o.value), ['', 'c1', 'c2']);
  assert.equal(els.from.children[1].textContent, 'c1 (before upgrade)');
  sent.length = 0;
  els['fork-id'].value = 'b2';
  els.from.value = '';
  await els.fork.handlers.submit({ preventDefault() {} });
  els['fork-id'].value = 'b3';
  els.from.value = 'c2';
  await els.fork.handlers.submit({ preventDefault() {} });
  const forks = sent.filter((r) => r.method === 'POST').map((r) => r.body);
  assert.deepEqual(forks, [{ id: 'b2' }, { id: 'b3', checkpoint: 'c2' }]);
});

const EGRESS = {
  [`GET ${A}/egress`]: [[200, { id: 'a1', profiles: ['rust'], effective_hosts: 5, missing: ['gone'] }]],
  'GET /api/v1/egress/profiles': [[200, { profiles: [
    { name: 'node', error: 'mode 644' },
    { name: 'rust', hosts: 2 },
    { name: 'wall', hosts: 1, exclusive: true },
  ] }]],
};
const boxes = (els) => els.profiles.children.map((label) => label.children[0]);

test('egress shows the attached profiles and hosts; only egress:write may change them', async () => {
  const reader = await load({ scopes: ['agents:read'], answers: { ...EGRESS } });
  assert.equal(reader.els['egress-state'].textContent, 'Reaches 5 hosts · missing: gone');
  assert.deepEqual(boxes(reader.els).map((b) => [b.value, b.checked, b.disabled]),
    [['node', false, true], ['rust', true, true], ['wall', false, true]]);
  assert.equal(reader.els.profiles.children[0].children[1].textContent, ' node (error: mode 644)');
  assert.equal(reader.els.profiles.children[2].children[1].textContent, ' wall (1 host, exclusive)');
  assert.equal(reader.els['save-egress'].hidden, true);
  const writer = await load({ scopes: ['agents:read', 'egress:write'], answers: { ...EGRESS } });
  assert.deepEqual(boxes(writer.els).map((b) => b.disabled), [true, false, false], 'a broken profile stays disabled');
  assert.equal(writer.els['save-egress'].hidden, false);
});

test('saving egress sends the ticked profiles; [] detaches all', async () => {
  const { els, sent } = await load({
    scopes: ['egress:write'],
    answers: { ...EGRESS, [`PUT ${A}/egress`]: [[200, { id: 'a1', profiles: ['wall'], effective_hosts: 1 }]] },
  });
  sent.length = 0;
  const [, rust, wall] = boxes(els);
  rust.checked = false;
  wall.checked = true;
  await els.egress.handlers.submit({ preventDefault() {} });
  assert.deepEqual([sent[0].method, sent[0].url, sent[0].body], ['PUT', `${A}/egress`, { profiles: ['wall'] }]);
  assert.equal(els.note.textContent, 'Egress: done.');
  for (const b of boxes(els)) b.checked = false;
  sent.length = 0;
  await els.egress.handlers.submit({ preventDefault() {} });
  assert.deepEqual(sent[0].body, { profiles: [] });
});

test('without the egress routes the section says why and offers no form', async () => {
  const { els } = await load({ scopes: ['agents:read'] });
  assert.equal(els['egress-state'].textContent, 'Egress: not found');
  assert.equal(els.egress.hidden, true);
});

test('the viewer links show only for viewer:read, and name the agent', async () => {
  const viewer = await load({ scopes: ['agents:read', 'viewer:read'] });
  assert.deepEqual([viewer.els.term.href, viewer.els.desk.href], ['/ui/a1', '/ui/desk/a1']);
  assert.deepEqual([viewer.els.term.hidden, viewer.els.desk.hidden], [false, false]);
  const other = await load({ scopes: ['agents:read'] });
  assert.deepEqual([other.els.term.hidden, other.els.desk.hidden], [true, true]);
});
