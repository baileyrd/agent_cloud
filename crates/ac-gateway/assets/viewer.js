// agent-cloud terminal viewer (ADR-006). The token lives only in this closure: never in storage,
// the URL or a cookie. Screen content arrives as sanitised runs ([text, fg, bg, attrs]) and is
// set with textContent, so nothing an agent prints is ever parsed as HTML.
(() => {
  'use strict';

  const agent = decodeURIComponent(location.pathname.split('/').pop());
  const base = `/api/v1/agents/${encodeURIComponent(agent)}/term`;
  const $ = (id) => document.getElementById(id);
  const term = $('term');
  const screen = $('screen');
  const cursor = $('cursor');
  const statusEl = $('status');

  let token = null;
  let who = null;
  let viewer = null;
  let owner = null;
  let rows = [];
  let note = '';
  let chain = Promise.resolve();
  let typed = '';
  let typedTimer = 0;

  // xterm's 16 colours, the 6x6x6 cube and the grey ramp.
  const PALETTE = ['#000000', '#cd0000', '#00cd00', '#cdcd00', '#3b78ff', '#cd00cd', '#00cdcd', '#e5e5e5',
    '#7f7f7f', '#ff0000', '#00ff00', '#ffff00', '#5c5cff', '#ff00ff', '#00ffff', '#ffffff'];
  const hex = (n) => n.toString(16).padStart(2, '0');
  for (let i = 0; i < 216; i++) {
    const level = (v) => (v ? v * 40 + 55 : 0);
    PALETTE.push(`#${hex(level(Math.floor(i / 36)))}${hex(level(Math.floor(i / 6) % 6))}${hex(level(i % 6))}`);
  }
  for (let i = 0; i < 24; i++) {
    const v = i * 10 + 8;
    PALETTE.push(`#${hex(v)}${hex(v)}${hex(v)}`);
  }
  const colour = (v) => (v < 0 ? '' : v < 256 ? PALETTE[v] : `#${(v & 0xffffff).toString(16).padStart(6, '0')}`);

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const controlling = () => viewer !== null && owner === viewer;
  // The server refuses take-over and input without viewer:control; this only hides the buttons.
  const mayControl = () => who !== null && (who.scopes.includes('viewer:control') || who.scopes.includes('admin'));

  function renderLine(div, runs) {
    div.replaceChildren(...runs.map(([text, fg, bg, attrs]) => {
      const span = document.createElement('span');
      span.textContent = text;
      let f = colour(fg);
      let b = colour(bg);
      if (attrs & 16) [f, b] = [b || 'var(--bg)', f || 'var(--fg)'];
      if (f) span.style.color = f;
      if (b) span.style.backgroundColor = b;
      if (attrs & 1) span.style.fontWeight = 'bold';
      if (attrs & 2) span.style.opacity = '0.6';
      if (attrs & 4) span.style.fontStyle = 'italic';
      const deco = [attrs & 8 ? 'underline' : '', attrs & 64 ? 'line-through' : ''].filter(Boolean);
      if (deco.length) span.style.textDecoration = deco.join(' ');
      if (attrs & 32) span.style.visibility = 'hidden';
      return span;
    }));
  }

  function place(m) {
    cursor.hidden = !m.cv;
    cursor.style.left = `${m.cx}ch`;
    cursor.style.top = `calc(${m.cy} * var(--lh))`;
  }

  function full(m) {
    const scroll = $('scroll');
    scroll.replaceChildren(...m.scroll.map((runs) => {
      const div = document.createElement('div');
      renderLine(div, runs);
      return div;
    }));
    rows = m.lines.map((runs) => {
      const div = document.createElement('div');
      div.className = 'row';
      renderLine(div, runs);
      return div;
    });
    screen.replaceChildren(...rows, cursor);
    place(m);
    term.scrollTop = term.scrollHeight;
  }

  function delta(m) {
    for (const [i, runs] of m.lines) {
      if (rows[i]) renderLine(rows[i], runs);
    }
    place(m);
  }

  function show() {
    let text;
    let cls = '';
    if (viewer === null) text = note || 'Connecting…';
    else if (controlling()) { text = 'You have control: keys go to the agent.'; cls = 'control'; }
    else if (owner) { text = `Viewer ${owner.slice(0, 6)} has control. View only.`; cls = 'other'; }
    else text = 'View only.';
    if (note && viewer !== null) text += ` ${note}`;
    statusEl.textContent = text;
    statusEl.className = cls;
    $('take').hidden = !mayControl() || viewer === null || controlling() || owner !== null;
    $('force').hidden = !mayControl() || viewer === null || controlling() || owner === null;
    $('release').hidden = !controlling();
    $('who').textContent = who ? `Token ${who.name}: ${who.scopes.join(', ')}` : '';
  }

  function handle(m) {
    switch (m.t) {
      case 'hello': viewer = m.viewer; note = ''; break;
      case 'mode': owner = m.owner; if (controlling()) resize(); break;
      case 'full': full(m); break;
      case 'delta': delta(m); break;
      case 'bye': note = `Session ended: ${m.reason}.`; break;
      default: return;
    }
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

  // Input is sent in order: each request waits for the one before it.
  function enqueue(path, body) {
    const v = viewer;
    chain = chain.then(async () => {
      if (v !== viewer) return;
      const r = await post(path, body);
      if (!r.ok && r.status !== 403) {
        note = `Input refused (${r.status}).`;
        show();
      }
    }).catch(() => {});
  }

  function flushTyped() {
    clearTimeout(typedTimer);
    if (typed) enqueue('text', typed);
    typed = '';
  }

  const NAMED = {
    Enter: 'Enter', Backspace: 'BSpace', Escape: 'Escape', ArrowUp: 'Up', ArrowDown: 'Down',
    ArrowLeft: 'Left', ArrowRight: 'Right', Home: 'Home', End: 'End', PageUp: 'PPage',
    PageDown: 'NPage', Insert: 'IC', Delete: 'DC',
  };

  function keyOf(e) {
    if (e.isComposing || e.metaKey) return null;
    const mods = (e.ctrlKey ? 'C-' : '') + (e.altKey ? 'M-' : '');
    if (e.key === 'Tab') return { key: e.shiftKey ? 'BTab' : `${mods}Tab` };
    if (NAMED[e.key]) return { key: mods + (e.shiftKey && e.key.startsWith('Arrow') ? 'S-' : '') + NAMED[e.key] };
    if (/^F([1-9]|1[0-2])$/.test(e.key)) return { key: mods + e.key };
    if ([...e.key].length !== 1) return null;
    if (e.ctrlKey || e.altKey) return /^[a-z0-9]$/i.test(e.key) ? { key: mods + e.key.toLowerCase() } : null;
    return { text: e.key };
  }

  document.addEventListener('keydown', (e) => {
    if (!controlling() || e.target instanceof HTMLButtonElement) return;
    const k = keyOf(e);
    if (!k) return;
    e.preventDefault();
    if (k.text) {
      typed += k.text;
      clearTimeout(typedTimer);
      typedTimer = setTimeout(flushTyped, 15);
    } else {
      flushTyped();
      enqueue('keys', k.key);
    }
  });

  document.addEventListener('paste', (e) => {
    if (!controlling()) return;
    e.preventDefault();
    const text = e.clipboardData.getData('text/plain');
    if (text) {
      flushTyped();
      enqueue('paste', text);
    }
  });

  function resize() {
    if (!controlling()) return;
    const m = $('measure').getBoundingClientRect();
    const cw = m.width / 10;
    const lh = m.height;
    const cols = Math.max(20, Math.min(500, Math.floor((term.clientWidth - 32) / cw)));
    const lines = Math.max(5, Math.min(200, Math.floor((term.clientHeight - 16) / lh)));
    enqueue(`resize?cols=${cols}&rows=${lines}`, '');
  }

  let resizeTimer = 0;
  window.addEventListener('resize', () => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(resize, 200);
  });

  async function lock(path) {
    const r = await post(path, '');
    if (r.status === 409) note = (await r.json()).error;
    else note = '';
    show();
    term.focus();
  }
  $('take').addEventListener('click', () => lock('takeover'));
  $('force').addEventListener('click', () => lock('takeover?force=true'));
  $('release').addEventListener('click', () => lock('release'));

  async function connect() {
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
          note = 'Wrong token.';
          show();
          continue;
        }
        if (r.status === 403 || r.status === 404) {
          note = r.status === 403 ? 'This token may not view this agent (needs viewer:read).' : 'No such agent for this token.';
          show();
          return;
        }
        if (!r.ok) throw new Error(`HTTP ${r.status}: ${(await r.json()).error}`);
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
      await sleep(2000);
    }
  }

  $('agent').textContent = agent;
  document.title = `${agent}: agent viewer`;
  show();
  connect();
})();
