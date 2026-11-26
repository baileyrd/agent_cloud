// agent-cloud desktop viewer (ADR-006, desktop addendum). The token lives only in this closure.
// Frames arrive as base64 JPEGs and are drawn on a canvas from a Blob, so nothing an agent shows
// is parsed as HTML. Input goes out only while this viewer holds the take-over lock; the gateway
// refuses it otherwise and accepts only coordinates, buttons and X key names.
(() => {
  'use strict';

  const agent = decodeURIComponent(location.pathname.split('/').pop());
  const base = `/api/v1/agents/${encodeURIComponent(agent)}/desk`;
  const $ = (id) => document.getElementById(id);
  const desk = $('desk');
  const canvas = $('frame');
  const statusEl = $('status');

  let token = null;
  let who = null;
  let viewer = null;
  let owner = null;
  let note = '';
  let chain = Promise.resolve();
  // The latest pointer position not yet sent: moves are coalesced, one request in flight.
  let move = null;
  let moving = false;
  // Whether this connection has shown a frame: a stream that ends before one (no ffmpeg in the
  // agent's image, say) is retried with a growing wait instead of every 2 s.
  let framed = false;
  const RETRY_MS = 2000;
  const RETRY_MAX_MS = 60000;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const controlling = () => viewer !== null && owner === viewer;
  const mayControl = () => who !== null && (who.scopes.includes('viewer:control') || who.scopes.includes('admin'));

  // Browser key names to X key names; anything else that prints is typed as text.
  const KEYS = {
    Enter: 'Return', Backspace: 'BackSpace', Tab: 'Tab', Escape: 'Escape', Delete: 'Delete',
    Insert: 'Insert', Home: 'Home', End: 'End', PageUp: 'Prior', PageDown: 'Next',
    ArrowLeft: 'Left', ArrowRight: 'Right', ArrowUp: 'Up', ArrowDown: 'Down', ' ': 'space',
  };
  for (let i = 1; i <= 12; i++) KEYS[`F${i}`] = `F${i}`;

  // [route, body] for a keydown, or null to leave it to the browser.
  function keyOf(e) {
    let name = KEYS[e.key];
    // Shift is already in a printed character; for a named key (Shift+Tab) it is a modifier.
    const shift = e.shiftKey && name !== undefined;
    const mods = [e.ctrlKey && 'ctrl', e.altKey && 'alt', e.metaKey && 'super', shift && 'shift'].filter(Boolean);
    if (!name && /^[A-Za-z0-9]$/.test(e.key)) name = e.key;
    if (mods.length) return name ? ['keys', [...mods, name].join('+')] : null;
    if (name && name.length > 1) return ['keys', name];
    if (e.key.length === 1) return ['text', e.key];
    return null;
  }

  function show() {
    const c = controlling();
    statusEl.textContent = note || (c ? 'You have control.' : owner ? `Viewer ${owner} has control.` : 'Watching (view-only).');
    statusEl.className = c ? 'control' : owner ? 'other' : '';
    const can = mayControl() && viewer !== null;
    $('take').hidden = !can || c || owner !== null;
    $('force').hidden = !can || c || owner === null;
    $('release').hidden = !c;
    $('who').textContent = who ? `as ${who.name}` : '';
  }

  async function frame(b64) {
    const bin = atob(b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    const image = await createImageBitmap(new Blob([bytes], { type: 'image/jpeg' }));
    if (canvas.width !== image.width) canvas.width = image.width;
    if (canvas.height !== image.height) canvas.height = image.height;
    canvas.getContext('2d').drawImage(image, 0, 0);
    image.close();
  }

  function handle(m) {
    if (m.t === 'hello') viewer = m.viewer;
    else if (m.t === 'mode') owner = m.owner;
    else if (m.t === 'frame') {
      framed = true;
      frame(m.jpeg).catch(() => {});
    }
    else if (m.t === 'bye') note = `Stream ended: ${m.reason}`;
    show();
  }

  function post(path, body) {
    return fetch(`${base}/${viewer}/${path}`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${token}` },
      body,
      cache: 'no-store',
    });
  }

  // Input requests go one at a time, in order.
  function enqueue(path, body) {
    if (!controlling()) return;
    chain = chain.then(() => post(path, body)).then((r) => {
      if (r.status === 403) owner = null;
      show();
    }, () => {});
  }

  // Canvas pixels for a mouse event: the screen is drawn scaled to fit.
  function at(e) {
    const r = canvas.getBoundingClientRect();
    const sx = canvas.width / (r.width || 1);
    const sy = canvas.height / (r.height || 1);
    const x = Math.max(0, Math.min(canvas.width - 1, Math.round((e.clientX - r.left) * sx)));
    const y = Math.max(0, Math.min(canvas.height - 1, Math.round((e.clientY - r.top) * sy)));
    return `${x} ${y}`;
  }

  // Send the latest pointer position in the input order, one move in flight; moves made meanwhile
  // are coalesced into the next.
  function flushMove() {
    if (moving || move === null || !controlling()) return;
    const xy = move;
    move = null;
    moving = true;
    chain = chain.then(() => post('pointer', `move ${xy}`)).catch(() => {}).then(() => {
      moving = false;
      flushMove();
    });
  }

  canvas.addEventListener('mousemove', (e) => {
    if (!controlling()) return;
    move = at(e);
    flushMove();
  });
  canvas.addEventListener('mousedown', (e) => {
    if (!controlling()) return;
    e.preventDefault();
    desk.focus();
    move = null;
    enqueue('pointer', `move ${at(e)}`);
    enqueue('pointer', `down ${e.button + 1}`);
  });
  canvas.addEventListener('mouseup', (e) => {
    if (controlling()) enqueue('pointer', `up ${e.button + 1}`);
  });
  canvas.addEventListener('contextmenu', (e) => { if (controlling()) e.preventDefault(); });
  canvas.addEventListener('wheel', (e) => {
    if (!controlling()) return;
    e.preventDefault();
    enqueue('pointer', `click ${e.deltaY < 0 ? 4 : 5}`);
  });
  desk.addEventListener('keydown', (e) => {
    if (!controlling()) return;
    const k = keyOf(e);
    if (!k) return;
    e.preventDefault();
    enqueue(k[0], k[1]);
  });

  async function lock(path) {
    const r = await post(path, '');
    note = r.status === 409 ? (await r.json()).error : '';
    show();
    desk.focus();
  }
  $('take').addEventListener('click', () => lock('takeover'));
  $('force').addEventListener('click', () => lock('takeover?force=true'));
  $('release').addEventListener('click', () => lock('release'));

  async function connect() {
    let wait = RETRY_MS;
    for (;;) {
      if (!token) {
        token = window.prompt(`Gateway token for ${agent}`);
        if (!token) {
          note = 'No token. Reload to try again.';
          show();
          return;
        }
      }
      try {
        const auth = { headers: { Authorization: `Bearer ${token}` }, cache: 'no-store' };
        if (!who) {
          const w = await fetch('/api/v1/whoami', auth);
          if (w.status === 401) {
            token = null;
            note = 'Wrong token.';
            show();
            continue;
          }
          if (!w.ok) throw new Error(`HTTP ${w.status}`);
          who = await w.json();
        }
        const r = await fetch(base, auth);
        if (r.status === 401) {
          token = null;
          who = null;
          continue;
        }
        if (r.status === 403 || r.status === 404) {
          note = r.status === 403 ? 'This token may not view this agent (needs viewer:read).' : 'No desktop for this agent or token.';
          show();
          return;
        }
        if (!r.ok) throw new Error(`HTTP ${r.status}`);
        note = '';
        const reader = r.body.getReader();
        const decoder = new TextDecoder();
        let buf = '';
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buf += decoder.decode(value, { stream: true });
          let nl;
          while ((nl = buf.indexOf('\n')) >= 0) {
            const line = buf.slice(0, nl);
            buf = buf.slice(nl + 1);
            if (line) handle(JSON.parse(line));
          }
        }
        if (!note) note = 'Disconnected.';
      } catch (err) {
        note = `Disconnected (${err.message}).`;
      }
      viewer = null;
      owner = null;
      show();
      wait = framed ? RETRY_MS : Math.min(wait * 2, RETRY_MAX_MS);
      framed = false;
      await sleep(wait);
    }
  }

  $('agent').textContent = agent;
  $('other').href = `/ui/${encodeURIComponent(agent)}`;
  document.title = `${agent}: agent desktop`;
  show();
  connect();
})();
