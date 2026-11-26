// The admin page (ADR-031, #516): API tokens and backups, for a token with admin for every agent.
// Any other token is told so and sees nothing: the gateway answers 404 to it on these routes anyway.
// A new token's secret is shown once and dropped from the page at the next action that goes ahead
// (a cancelled confirm keeps it).
(() => {
  'use strict';

  const { api, signIn, may, el, errorText } = window.UI;
  const $ = (id) => document.getElementById(id);
  let me = ''; // the signed-in token's name
  const SCOPES = [
    'agents:read', 'agents:write', 'jobs:read', 'jobs:write',
    'viewer:read', 'viewer:control', 'egress:write', 'admin',
  ];

  function note(text, bad = false) {
    $('note').textContent = text;
    $('note').className = bad ? 'bad' : '';
  }

  // Unix seconds as `YYYY-MM-DD HH:MM UTC`; `none` for null.
  const when = (secs, none = '') => (secs == null ? none
    : `${new Date(secs * 1000).toISOString().slice(0, 16).replace('T', ' ')} UTC`);

  // Bytes for a person, as `ac-ctl` prints them.
  function size(bytes) {
    for (const [unit, n] of [['GiB', 2 ** 30], ['MiB', 2 ** 20], ['KiB', 2 ** 10]]) {
      if (bytes >= n) return `${(bytes / n).toFixed(1)} ${unit}`;
    }
    return `${bytes} B`;
  }

  // Drop a shown secret: it is shown once, until the next action.
  function forget() {
    $('secret').hidden = true;
    $('secret-text').textContent = '';
    $('secret-name').textContent = '';
  }

  function button(label, onclick) {
    return el('button', label, { type: 'button', onclick });
  }

  function tokenRow(t) {
    const expires = t.live ? when(t.expires, 'never') : `expired ${when(t.expires)}`;
    const actions = el('td', undefined, { className: 'actions' });
    actions.append(button('Revoke', () => revoke(t.name)));
    const tr = el('tr');
    tr.append(
      el('td', t.name),
      el('td', t.scopes.join(', ')),
      el('td', Array.isArray(t.agents) ? t.agents.join(', ') : t.agents),
      el('td', when(t.created)),
      el('td', expires),
      actions,
    );
    return tr;
  }

  async function tokens() {
    const r = await api('GET', '/api/v1/tokens');
    if (!r.ok) {
      $('tokens').replaceChildren();
      note(`Tokens: ${errorText(r)}`, true);
      return;
    }
    $('tokens').replaceChildren(...r.data.tokens.map(tokenRow));
  }

  async function revoke(name) {
    const own = name === me ? ' It is the token this page signed in with: the page stops working too.' : '';
    if (!window.confirm(`Revoke token ${name}? Anything using it stops working at once.${own}`)) return;
    forget();
    const r = await api('DELETE', '/api/v1/tokens/{name}', { name });
    note(r.ok ? `Revoked ${name}.` : `Revoke ${name}: ${errorText(r)}`, !r.ok);
    await tokens();
  }

  // The create form's body, or a reason it cannot be sent.
  function grant() {
    const scopes = SCOPES.filter((s) => $(`scope-${s}`).checked);
    if (scopes.length === 0) return { why: 'Pick at least one scope.' };
    const list = $('new-agents').value.trim();
    const agents = list === '*' ? '*' : list.split(',').map((a) => a.trim()).filter(Boolean);
    if (agents.length === 0) return { why: 'Name the agents, or * for all.' };
    const body = { scopes, agents };
    const expires = $('new-expires').value.trim();
    if (expires) body.expires = expires;
    return { body };
  }

  async function create(event) {
    event.preventDefault();
    forget();
    const name = $('new-name').value.trim();
    const { body, why } = grant();
    if (!name || why) {
      note(why ?? 'Name the token.', true);
      return;
    }
    const r = await api('POST', '/api/v1/tokens/{name}', { name }, { body });
    if (!r.ok) {
      note(`Create ${name}: ${errorText(r)}`, true);
      return;
    }
    note(`Created ${name}.`);
    $('secret-name').textContent = r.data.name;
    $('secret-text').textContent = r.data.token;
    $('secret').hidden = false;
    $('new-name').value = '';
    await tokens();
  }

  async function copy() {
    try {
      await window.navigator.clipboard.writeText($('secret-text').textContent);
      note('Copied.');
    } catch (e) {
      note(`Copy failed (${e.message ?? e}): select the text and copy it.`, true);
    }
  }

  function runText(run) {
    if (['active', 'activating', 'reloading'].includes(run.active)) {
      return `A backup is running (started ${when(run.started)}).`;
    }
    if (run.started == null) return 'No backup has run since boot.';
    return `Last run: ${run.result || run.active}, started ${when(run.started)}, finished ${when(run.finished)}.`;
  }

  function streamRow(agent, b, paused) {
    const kind = b.parent == null ? 'full' : `incremental from ${b.parent}`;
    const actions = el('td', undefined, { className: 'actions' });
    if (!b.restorable) actions.append(el('span', 'chain broken'));
    if (b.restorable && paused) actions.append(button('Restore', () => restore(agent, b.id)));
    const tr = el('tr');
    tr.append(el('td', agent), el('td', b.id), el('td', kind), el('td', size(b.bytes)), actions);
    return tr;
  }

  async function backups() {
    const r = await api('GET', '/api/v1/backups');
    $('backups').hidden = !r.ok;
    $('backups-off').hidden = r.ok;
    if (!r.ok) {
      $('backups-off').textContent = errorText(r);
      return;
    }
    const states = await api('GET', '/api/v1/agents');
    if (!states.ok) note(`Agents: ${errorText(states)}: no restore is offered until they list.`, true);
    const paused = new Set((states.ok ? states.data.agents : [])
      .filter((a) => a.state === 'paused').map((a) => a.id));
    $('run').textContent = runText(r.data.run);
    const state = r.data.state;
    $('state').textContent = state.length === 0 ? 'No state copies.'
      : `State copies: ${state.length}, newest ${state[state.length - 1]}.`;
    const rows = r.data.agents.flatMap((a) => a.backups.map((b) => streamRow(a.id, b, paused.has(a.id))));
    $('streams').replaceChildren(...rows);
  }

  async function backUp() {
    forget();
    const r = await api('POST', '/api/v1/backups');
    note(r.ok ? 'Backup started: it runs as the nightly one does.' : `Back up: ${errorText(r)}`, !r.ok);
    await backups();
  }

  async function restore(id, backup) {
    const ask = `Restore ${id} from backup ${backup}? Its workspace is kept as a checkpoint first.`;
    if (!window.confirm(ask)) return;
    forget();
    note(`Restoring ${id} from ${backup}: this takes as long as the transfer…`);
    const r = await api('POST', '/api/v1/agents/{id}/restore-backup', { id }, { body: { backup } });
    const kept = r.ok && r.data.kept ? ` The old workspace is checkpoint ${r.data.kept}.` : '';
    note(r.ok ? `Restored ${id} from ${backup}.${kept}` : `Restore ${id}: ${errorText(r)}`, !r.ok);
    await backups();
  }

  async function refresh() {
    forget();
    await tokens();
    await backups();
  }

  function scopeBoxes() {
    return SCOPES.map((s) => {
      const label = el('label', undefined);
      label.append(el('input', undefined, { type: 'checkbox', id: `scope-${s}` }), el('span', s));
      return label;
    });
  }

  async function start() {
    const who = await signIn('Gateway token (admin)');
    if (!who) {
      note('No token. Reload to try again.', true);
      return;
    }
    me = who.name;
    $('who').textContent = `${who.name} (${who.scopes.join(', ')})`;
    if (!may('admin') || who.agents !== '*') {
      note('This page needs a token with admin for every agent.', true);
      return;
    }
    $('scopes').replaceChildren(...scopeBoxes());
    $('admin').hidden = false;
    $('refresh').hidden = false;
    $('create').addEventListener('submit', create);
    $('copy').addEventListener('click', copy);
    $('back-up').addEventListener('click', backUp);
    $('refresh').addEventListener('click', refresh);
    await refresh();
  }

  start().catch((e) => note(String(e.message ?? e), true));
})();
