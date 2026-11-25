// Input handling of assets/desk.js, in a stub DOM with node's own test runner (no dependencies):
// `scripts/ci-local.sh web`. The page takes control of a fake gateway; each test feeds it events
// and checks the input requests it sends.
import { test, before } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const SCRIPT = readFileSync(new URL('../assets/desk.js', import.meta.url), 'utf8');
const sent = [];
const els = {};

function element() {
  const listeners = {};
  return {
    listeners,
    style: {},
    hidden: false,
    textContent: '',
    className: '',
    width: 1920,
    height: 1080,
    addEventListener(type, fn) { listeners[type] = fn; },
    focus() {},
    // Drawn at half size, 100 px from the left.
    getBoundingClientRect: () => ({ left: 100, top: 0, width: 960, height: 540 }),
  };
}

function stream() {
  const lines = ['{"t":"hello","viewer":"v1"}', '{"t":"mode","owner":"v1"}'];
  const bytes = new TextEncoder().encode(`${lines.join('\n')}\n`);
  let done = false;
  return {
    getReader: () => ({
      read: () => (done ? new Promise(() => {}) : ((done = true), Promise.resolve({ value: bytes, done: false }))),
    }),
  };
}

async function fetchStub(url, init = {}) {
  if (init.method === 'POST') {
    sent.push([url.split('/').pop().split('?')[0], init.body]);
    return { ok: true, status: 200 };
  }
  if (url === '/api/v1/whoami') return { ok: true, status: 200, json: async () => ({ name: 't', scopes: ['viewer:control'] }) };
  return { ok: true, status: 200, body: stream() };
}

const tick = () => new Promise((r) => setTimeout(r, 40));

before(async () => {
  const document = { title: '', getElementById: (id) => (els[id] ??= element()) };
  runInNewContext(SCRIPT, {
    document,
    location: { pathname: '/ui/desk/a1' },
    window: { prompt: () => 'token' },
    fetch: fetchStub,
    TextEncoder,
    TextDecoder,
    setTimeout,
    Promise,
  });
  await tick();
});

async function fire(el, type, init) {
  sent.length = 0;
  let prevented = false;
  els[el].listeners[type]({ ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, ...init,
    preventDefault: () => { prevented = true; } });
  await tick();
  return [prevented, [...sent]];
}

test('keys go out as X names, printable characters as text', async () => {
  assert.deepEqual(await fire('desk', 'keydown', { key: 'Enter' }), [true, [['keys', 'Return']]]);
  assert.deepEqual(await fire('desk', 'keydown', { key: 'a' }), [true, [['text', 'a']]]);
  assert.deepEqual(await fire('desk', 'keydown', { key: '!' }), [true, [['text', '!']]]);
  assert.deepEqual(await fire('desk', 'keydown', { key: ' ' }), [true, [['keys', 'space']]]);
  assert.deepEqual(await fire('desk', 'keydown', { key: 'c', ctrlKey: true }), [true, [['keys', 'ctrl+c']]]);
  assert.deepEqual(await fire('desk', 'keydown', { key: 'F5' }), [true, [['keys', 'F5']]]);
});

test('lone modifiers and unknown chords are left to the browser', async () => {
  assert.deepEqual(await fire('desk', 'keydown', { key: 'Control', ctrlKey: true }), [false, []]);
  assert.deepEqual(await fire('desk', 'keydown', { key: '!', ctrlKey: true }), [false, []]);
});

test('the pointer is scaled from the drawn size to screen pixels', async () => {
  const [prevented, got] = await fire('frame', 'mousedown', { clientX: 150, clientY: 20, button: 0 });
  assert.equal(prevented, true);
  assert.deepEqual(got, [['pointer', 'move 100 40'], ['pointer', 'down 1']]);
  assert.deepEqual((await fire('frame', 'mouseup', { button: 2 }))[1], [['pointer', 'up 3']]);
  assert.deepEqual((await fire('frame', 'wheel', { deltaY: 3 }))[1], [['pointer', 'click 5']]);
  assert.deepEqual((await fire('frame', 'mousemove', { clientX: 5000, clientY: -9 }))[1], [['pointer', 'move 1919 0']]);
});
