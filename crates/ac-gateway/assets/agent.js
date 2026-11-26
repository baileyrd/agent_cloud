// One agent's page (ADR-030 slice 3, #516): its state, and its workspace checkpoints (ADR-018):
// make one, delete one, restore one onto the paused agent, and fork a new agent from the workspace
// now or from a checkpoint; with slice 5, its egress profiles (ADR-016): which are attached, how
// many hosts that lets it reach, and (with egress:write) replacing them. Controls the token's
// scopes cannot use are hidden or disabled.
(() => {
  'use strict';

  const { api, signIn, may, el, errorText } = window.UI;
  const $ = (id) => document.getElementById(id);
  const agent = decodeURIComponent(location.pathname.split('/').pop());
  const ids = { id: agent };

  // The agent as last read: restore needs it paused.
  let state = '';
  // The egress checkboxes as last drawn.
  let boxes = [];

  function note(text, bad = false) {
    $('note').textContent = text;
    $('note').className = bad ? 'bad' : '';
  }

  // Run one change, report it, and read the page again.
  async function change(what, call) {
    const r = await call();
    note(r.ok ? `${what}: done.` : `${what}: ${errorText(r)}`, !r.ok);
    await refresh();
    return r.ok;
  }

  function restore(cp) {
    const ask = `Restore ${agent} to checkpoint ${cp}? The workspace as it is now is kept as a new checkpoint.`;
    if (!window.confirm(ask)) return Promise.resolve(false);
    return change(`Restore ${cp}`, () => api('POST', '/api/v1/agents/{id}/restore', ids, { body: { checkpoint: cp } }));
  }

  function remove(cp) {
    if (!window.confirm(`Delete checkpoint ${cp} of ${agent}?`)) return Promise.resolve(false);
    return change(`Delete ${cp}`, () => api('DELETE', '/api/v1/agents/{id}/checkpoints/{cp}', { ...ids, cp }));
  }

  function row(cp) {
    const actions = el('td', undefined, { className: 'actions' });
    if (may('agents:write')) {
      if (state === 'paused') {
        actions.append(el('button', 'Restore', { type: 'button', onclick: () => restore(cp.id) }));
      }
      actions.append(el('button', 'Delete', { type: 'button', onclick: () => remove(cp.id) }));
    }
    const tr = el('tr');
    tr.append(el('td', cp.id), el('td', cp.label ?? ''), actions);
    return tr;
  }

  // The fork form's checkpoint choice: the workspace now, or one of the checkpoints.
  function choices(checkpoints) {
    const now = el('option', 'the workspace now', { value: '' });
    return [now, ...checkpoints.map((cp) => el('option', cp.label ? `${cp.id} (${cp.label})` : cp.id, { value: cp.id }))];
  }

  async function refresh() {
    const a = await api('GET', '/api/v1/agents/{id}', ids);
    if (!a.ok) {
      note(`${agent}: ${errorText(a)}`, true);
      return;
    }
    state = a.data.state;
    $('state').textContent = a.data.health ? `${state} · ${a.data.health}` : state;
    const r = await api('GET', '/api/v1/agents/{id}/checkpoints', ids);
    if (!r.ok) {
      note(`Checkpoints: ${errorText(r)}`, true);
      return;
    }
    $('rows').replaceChildren(...r.data.checkpoints.map(row));
    $('count').textContent = `${r.data.checkpoints.length} of ${r.data.max}`;
    $('from').replaceChildren(...choices(r.data.checkpoints));
    $('restore-hint').hidden = state === 'paused' || !may('agents:write');
    await egress();
  }

  // One checkbox per operator profile, ticked if attached. A profile that failed to load is shown
  // with its error and cannot be picked.
  function profileBox(p, attached) {
    const box = el('input', undefined, {
      type: 'checkbox',
      value: p.name,
      checked: attached.includes(p.name),
      disabled: !may('egress:write') || p.error !== undefined,
    });
    const detail = p.error !== undefined
      ? `error: ${p.error}`
      : `${p.hosts} host${p.hosts === 1 ? '' : 's'}${p.exclusive ? ', exclusive' : ''}`;
    boxes.push(box);
    const label = el('label');
    label.append(box, el('span', ` ${p.name} (${detail})`));
    return label;
  }

  async function egress() {
    const mine = await api('GET', '/api/v1/agents/{id}/egress', ids);
    const all = await api('GET', '/api/v1/egress/profiles');
    if (!mine.ok || !all.ok) {
      $('egress-state').textContent = `Egress: ${errorText(mine.ok ? all : mine)}`;
      $('profiles').replaceChildren();
      $('egress').hidden = true;
      return;
    }
    const missing = mine.data.missing?.length ? ` · missing: ${mine.data.missing.join(', ')}` : '';
    $('egress-state').textContent = `Reaches ${mine.data.effective_hosts} hosts${missing}`;
    boxes = [];
    $('profiles').replaceChildren(...all.data.profiles.map((p) => profileBox(p, mine.data.profiles)));
    $('egress').hidden = false;
    $('save-egress').hidden = !may('egress:write');
  }

  async function saveEgress(event) {
    event.preventDefault();
    const profiles = boxes.filter((b) => b.checked).map((b) => b.value);
    await change('Egress', () => api('PUT', '/api/v1/agents/{id}/egress', ids, { body: { profiles } }));
  }

  async function checkpoint(event) {
    event.preventDefault();
    const label = $('label').value.trim();
    const body = label ? { label } : {};
    if (await change('Checkpoint', () => api('POST', '/api/v1/agents/{id}/checkpoints', ids, { body }))) {
      $('label').value = '';
    }
  }

  async function fork(event) {
    event.preventDefault();
    const id = $('fork-id').value.trim();
    if (!id) return;
    const body = { id };
    if ($('from').value) body.checkpoint = $('from').value;
    if (await change(`Fork to ${id}`, () => api('POST', '/api/v1/agents/{id}/fork', ids, { body }))) {
      $('fork-id').value = '';
    }
  }

  async function start() {
    $('agent').textContent = agent;
    $('jobs').href = `/ui/jobs/${encodeURIComponent(agent)}`;
    const who = await signIn(`Gateway token for ${agent}`);
    if (!who) {
      note('No token. Reload to try again.', true);
      return;
    }
    $('who').textContent = `${who.name} (${who.scopes.join(', ')})`;
    for (const id of ['checkpoint', 'fork']) $(id).hidden = !may('agents:write');
    $('checkpoint').addEventListener('submit', checkpoint);
    $('fork').addEventListener('submit', fork);
    $('egress').addEventListener('submit', saveEgress);
    $('refresh').addEventListener('click', refresh);
    await refresh();
  }

  start().catch((e) => note(String(e.message ?? e), true));
})();
