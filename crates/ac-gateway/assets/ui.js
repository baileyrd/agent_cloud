// Shared by the management pages (ADR-030): the token prompt, the API call, whoami and an element
// helper. The token lives only in this closure: never in storage, the URL or a cookie. Every API
// call names its method and then the catalog's own path pattern ({id} segments), both as single-
// quoted literals side by side, so a test checks each pair against access::ROUTES and no page can
// grow a private route.
(() => {
  'use strict';

  let token = null;
  let who = null;

  // A catalog pattern with its `{name}` segments filled from `params` (URL-encoded), plus `query`.
  function url(pattern, params = {}, query = {}) {
    const path = pattern.replace(/\{(\w+)\}/g, (_, name) => {
      if (!(name in params)) throw new Error(`no value for {${name}} in ${pattern}`);
      return encodeURIComponent(params[name]);
    });
    const q = new URLSearchParams(query).toString();
    return q ? `${path}?${q}` : path;
  }

  // One API call. Resolves to { status, ok, data }; data is the JSON reply, or {} if it had none.
  async function api(method, pattern, params = {}, { query = {}, body } = {}) {
    if (!token) throw new Error('Not signed in.');
    const headers = { Authorization: `Bearer ${token}` };
    const init = { method, headers, cache: 'no-store' };
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }
    const r = await fetch(url(pattern, params, query), init);
    const data = await r.json().catch(() => ({}));
    return { status: r.status, ok: r.ok, data };
  }

  // Ask for a token until whoami accepts one. Resolves to whoami's reply, or null if dismissed.
  async function signIn(label) {
    for (;;) {
      token = window.prompt(label);
      if (!token) {
        token = null;
        return null;
      }
      const r = await api('GET', '/api/v1/whoami');
      if (r.ok) {
        who = r.data;
        return who;
      }
      token = null;
      if (r.status !== 401) throw new Error(`whoami: ${errorText(r)}`);
    }
  }

  // Whether the token holds `scope` (admin holds all). The gateway still checks every call: this
  // only hides controls the token could not use.
  const may = (scope) => who !== null && (who.scopes.includes(scope) || who.scopes.includes('admin'));

  // An element whose text, if any, is set with textContent: nothing is ever parsed as HTML.
  function el(tag, text, props = {}) {
    const e = document.createElement(tag);
    if (text !== undefined) e.textContent = String(text);
    return Object.assign(e, props);
  }

  const errorText = (r) => (typeof r.data.error === 'string' ? r.data.error : `HTTP ${r.status}`);

  window.UI = Object.freeze({ url, api, signIn, may, el, errorText });
})();
