// Keyboard and paste handling of assets/viewer.js, run in a stub DOM with node's own test runner
// (no dependencies): `scripts/ci-local.sh web`. The page takes control of a fake
// gateway, then each test feeds it events and checks the input requests it sends.
import { test, before } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const SCRIPT = readFileSync(new URL('../assets/viewer.js', import.meta.url), 'utf8');
const sent = [];
const listeners = {};

function element() {
  return {
    style: {},
    hidden: false,
    textContent: '',
    className: '',
    clientWidth: 800,
    clientHeight: 600,
    scrollHeight: 0,
    replaceChildren() {},
    addEventListener() {},
    setAttribute() {},
    focus() {},
    getBoundingClientRect: () => ({ width: 80, height: 16 }),
  };
}

// The term stream: hello, then this viewer takes control, then it stays open.
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
  const els = {};
  const document = {
    title: '',
    getElementById: (id) => (els[id] ??= element()),
    createElement: element,
    addEventListener: (type, fn) => { listeners[type] = fn; },
  };
  runInNewContext(SCRIPT, {
    document,
    location: { pathname: '/ui/a1' },
    window: { prompt: () => 'token', addEventListener() {} },
    fetch: fetchStub,
    TextEncoder,
    TextDecoder,
    setTimeout,
    clearTimeout,
    Promise,
    HTMLButtonElement: class {},
  });
  await tick();
  sent.length = 0; // the resize sent on taking control
});

// Feeds one keydown and returns [whether the page kept it from the browser, what it sent].
async function key(init) {
  sent.length = 0;
  let prevented = false;
  listeners.keydown({ ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, target: {}, ...init,
    preventDefault: () => { prevented = true; } });
  await tick();
  return [prevented, [...sent]];
}

test('Ctrl+V and Ctrl+Shift+V are left to the browser, which pastes', async () => {
  assert.deepEqual(await key({ key: 'v', ctrlKey: true }), [false, []]);
  assert.deepEqual(await key({ key: 'V', ctrlKey: true, shiftKey: true }), [false, []]);
});

test('the browser paste sends the clipboard text', async () => {
  sent.length = 0;
  let prevented = false;
  listeners.paste({ clipboardData: { getData: () => 'CODE-123' }, preventDefault: () => { prevented = true; } });
  await tick();
  assert.equal(prevented, true);
  assert.deepEqual(sent, [['paste', 'CODE-123']]);
});

test('Ctrl+Alt+V sends a literal ^V', async () => {
  assert.deepEqual(await key({ key: 'v', ctrlKey: true, altKey: true }), [true, [['keys', 'C-v']]]);
});

test('other control keys and typing still go to the terminal', async () => {
  assert.deepEqual(await key({ key: 'c', ctrlKey: true }), [true, [['keys', 'C-c']]]);
  assert.deepEqual(await key({ key: 'Enter' }), [true, [['keys', 'Enter']]]);
  assert.deepEqual(await key({ key: 'v' }), [true, [['text', 'v']]]);
});
