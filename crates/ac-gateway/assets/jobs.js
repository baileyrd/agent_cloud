// One agent's jobs (ADR-030 slice 2, #516): submit a prompt, the list with states, and for the job
// picked its live state (long-poll), cancel, and once it ends its output and witness (ADR-029).
// Output is agent-written: it is only ever set with textContent.
(() => {
  'use strict';

  const { api, signIn, may, el, errorText } = window.UI;
  const $ = (id) => document.getElementById(id);
  const agent = decodeURIComponent(location.pathname.split('/').pop());
  const ids = { id: agent };
  const ACTIVE = ['queued', 'running'];
  const WAIT_SECS = '25';
  // A poll that answers sooner than this is followed by a pause, so a gateway that answers at
  // once (no change to wait for, or an error page) is not polled in a tight loop.
  const POLL_FLOOR_MS = 1000;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // Bumped whenever another job is picked, so an older watch loop stops.
  let watch = 0;

  function note(text, bad = false) {
    $('note').textContent = text;
    $('note').className = bad ? 'bad' : '';
  }

  const when = (ms) => (ms ? new Date(ms).toLocaleString() : '');

  function summary(job) {
    const parts = [job.state];
    if (job.queued_reason) parts.push(`waiting: ${job.queued_reason}`);
    if (job.exit_code !== null && job.exit_code !== undefined) parts.push(`exit ${job.exit_code}`);
    if (job.reason) parts.push(job.reason);
    return parts.join(' · ');
  }

  async function cancel(job) {
    if (!window.confirm(`Cancel job ${job}?`)) return;
    const r = await api('POST', '/api/v1/agents/{id}/jobs/{job}/cancel', { ...ids, job });
    note(r.ok ? `Cancelled ${job}.` : `Cancel ${job}: ${errorText(r)}`, !r.ok);
    await refresh();
  }

  function row(job) {
    const actions = el('td', undefined, { className: 'actions' });
    actions.append(el('button', 'Show', { type: 'button', onclick: () => pick(job.job_id) }));
    if (may('jobs:write') && ACTIVE.includes(job.state)) {
      actions.append(el('button', 'Cancel', { type: 'button', onclick: () => cancel(job.job_id) }));
    }
    const tr = el('tr');
    tr.append(el('td', job.job_id), el('td', summary(job)), el('td', when(job.created_ms)), actions);
    return tr;
  }

  async function refresh() {
    const r = await api('GET', '/api/v1/agents/{id}/jobs', ids);
    if (!r.ok) {
      note(`Jobs: ${errorText(r)}`, true);
      return;
    }
    $('rows').replaceChildren(...r.data.jobs.map(row));
    $('empty').hidden = r.data.jobs.length > 0;
  }

  // The picked job's ended view: its final message and its witness record with digest.
  async function ended(job) {
    const out = await api('GET', '/api/v1/agents/{id}/jobs/{job}/output', { ...ids, job }, { text: true });
    $('output').textContent = out.ok ? out.data : `(no output: ${errorText(out)})`;
    const w = await api('GET', '/api/v1/agents/{id}/jobs/{job}/witness', { ...ids, job });
    $('witness').textContent = w.ok
      ? `${JSON.stringify(JSON.parse(w.data.record), null, 2)}\nsha256 ${w.data.sha256}`
      : `(no witness: ${errorText(w)})`;
  }

  // Follow one job: long-poll its state until it ends (or another is picked), then show the rest.
  async function pick(job) {
    const mine = ++watch;
    $('detail').hidden = false;
    $('picked').textContent = job;
    $('output').textContent = '';
    $('witness').textContent = '';
    let query = {};
    while (mine === watch) {
      const asked = Date.now();
      const r = await api('GET', '/api/v1/agents/{id}/jobs/{job}', { ...ids, job }, { query });
      if (mine !== watch) return;
      if (!r.ok) {
        $('state').textContent = errorText(r);
        return;
      }
      $('state').textContent = summary(r.data);
      if (!ACTIVE.includes(r.data.state)) {
        await refresh();
        await ended(job);
        return;
      }
      query = { wait: WAIT_SECS };
      const quick = POLL_FLOOR_MS - (Date.now() - asked);
      if (quick > 0) await sleep(quick);
    }
  }

  async function submit(event) {
    event.preventDefault();
    const prompt = $('prompt').value;
    if (!prompt.trim()) return;
    const body = { prompt };
    const secs = Number($('timeout').value);
    if (secs > 0) body.timeout_secs = secs;
    const r = await api('POST', '/api/v1/agents/{id}/jobs', ids, { body });
    if (!r.ok) {
      note(`Submit: ${errorText(r)}`, true);
      return;
    }
    note(`Submitted ${r.data.job_id}.`);
    $('prompt').value = '';
    await refresh();
    pick(r.data.job_id);
  }

  async function start() {
    $('agent').textContent = agent;
    const who = await signIn(`Gateway token for ${agent}'s jobs`);
    if (!who) {
      note('No token. Reload to try again.', true);
      return;
    }
    $('who').textContent = `${who.name} (${who.scopes.join(', ')})`;
    $('submit').hidden = !may('jobs:write');
    $('submit').addEventListener('submit', submit);
    $('refresh').addEventListener('click', refresh);
    await refresh();
  }

  start().catch((e) => note(String(e.message ?? e), true));
})();
