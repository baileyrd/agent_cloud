// The agents page (ADR-030 slice 1, #516): every agent the token is for, with its state and health,
// and create, pause, resume, destroy and purge; with slice 4, the host's capacity above the list. Controls the token's scopes cannot use are hidden.
(() => {
  'use strict';

  const { api, signIn, may, el, errorText } = window.UI;
  const $ = (id) => document.getElementById(id);
  const REFRESH_MS = 10000;

  // What each state offers. `force`: a 409 (the agent has a job) may be retried with ?force=true,
  // which interrupts the job, once the user confirms. `ask`: confirmed first; {id} is the agent.
  const ACTIONS = [
    { label: 'Pause', states: ['running'], force: true, call: ['POST', '/api/v1/agents/{id}/pause'] },
    { label: 'Resume', states: ['paused'], call: ['POST', '/api/v1/agents/{id}/resume'] },
    {
      label: 'Destroy',
      states: ['running', 'paused'],
      force: true,
      ask: 'Destroy {id}? Its VM stops for good; the workspace is kept.',
      call: ['DELETE', '/api/v1/agents/{id}'],
    },
    {
      label: 'Purge',
      states: ['running', 'paused', 'destroyed'],
      force: true,
      query: { purge: 'true' },
      ask: 'Purge {id}? This deletes its workspace and job records and cannot be undone.',
      call: ['DELETE', '/api/v1/agents/{id}'],
    },
  ];

  function note(text, bad = false) {
    $('note').textContent = text;
    $('note').className = bad ? 'bad' : '';
  }

  async function act(id, action) {
    if (action.ask && !window.confirm(action.ask.replace('{id}', id))) return;
    const [method, pattern] = action.call;
    const query = action.query ?? {};
    let r = await api(method, pattern, { id }, { query });
    if (r.status === 409 && action.force
      && window.confirm(`${errorText(r)}\n\n${action.label} ${id} anyway, interrupting the job?`)) {
      r = await api(method, pattern, { id }, { query: { ...query, force: 'true' } });
    }
    note(r.ok ? `${action.label} ${id}: done.` : `${action.label} ${id}: ${errorText(r)}`, !r.ok);
    await refresh();
  }

  // The agent's id, linking to its own page.
  function link(id) {
    const td = el('td');
    td.append(el('a', id, { href: `/ui/agent/${encodeURIComponent(id)}` }));
    return td;
  }

  function row(agent) {
    const buttons = el('td', undefined, { className: 'actions' });
    if (may('agents:write')) {
      for (const action of ACTIONS.filter((a) => a.states.includes(agent.state))) {
        buttons.append(el('button', action.label, { type: 'button', onclick: () => act(agent.id, action) }));
      }
    }
    const tr = el('tr');
    tr.append(
      link(agent.id),
      el('td', agent.state, { className: 'state' }),
      el('td', agent.health ?? ''),
      buttons,
    );
    return tr;
  }

  // The host's admission figures (ADR-012) and job slots, as one line.
  function capacity(c) {
    const parts = [
      `room for ${c.max_more_agents} more agent${c.max_more_agents === 1 ? '' : 's'}`,
      `${c.running_agents} running`,
      `/dev/shm ${c.shm_free_mib} of ${c.shm_total_mib} MiB free`,
      `${c.guest_mib} MiB per guest`,
    ];
    if (c.max_running_jobs !== undefined) {
      parts.push(`jobs: ${c.running_jobs} of ${c.max_running_jobs} running, ${c.queued_jobs} queued`);
    }
    return parts.join(' · ');
  }

  async function refresh() {
    const r = await api('GET', '/api/v1/agents');
    if (!r.ok) {
      note(`Agents: ${errorText(r)}`, true);
      return;
    }
    $('rows').replaceChildren(...r.data.agents.map(row));
    $('empty').hidden = r.data.agents.length > 0;
    const c = await api('GET', '/api/v1/capacity');
    $('capacity').hidden = !c.ok;
    $('capacity').textContent = c.ok ? capacity(c.data) : '';
  }

  async function create(event) {
    event.preventDefault();
    const id = $('new-id').value.trim();
    if (!id) return;
    note(`Creating ${id}…`);
    const r = await api('POST', '/api/v1/agents/{id}', { id });
    note(r.ok ? `Created ${id}.` : `Create ${id}: ${errorText(r)}`, !r.ok);
    if (r.ok) $('new-id').value = '';
    await refresh();
  }

  async function start() {
    const who = await signIn('Gateway token');
    if (!who) {
      note('No token. Reload to try again.', true);
      return;
    }
    $('who').textContent = `${who.name} (${who.scopes.join(', ')})`;
    $('create').hidden = !may('agents:write');
    $('create').addEventListener('submit', create);
    $('refresh').addEventListener('click', refresh);
    await refresh();
    setInterval(refresh, REFRESH_MS);
  }

  start().catch((e) => note(String(e.message ?? e), true));
})();
