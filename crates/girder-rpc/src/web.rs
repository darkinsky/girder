//! The smallest possible browser client, served by the same process.
//!
//! This is not the Web UI. It is the thing that proves the chain works: it opens in a browser, calls
//! the Remote methods over HTTP, and shows what came back. Building it now rather than after a real
//! front end is deliberate — a page that uses the API exposes its problems, and those are cheaper to
//! find here than after a build system is in the way.
//!
//! # Why the page is embedded
//!
//! A served directory would need a build step, a watch mode, and a path traversal check. An embedded
//! string needs none of those, and a single-file client has no bundler to configure.
//!
//! # What it deliberately does not do
//!
//! No streaming, no authentication, no state management. Each of those is a real need and each would
//! turn this into a different project. The page answers one question: does a browser talking to this
//! server see the same methods a test does?

/// The path the page is served at.
pub const WEB_INDEX_PATH: &str = "/";

/// The page, embedded.
///
/// A single file rather than a served directory: no build step, no traversal check, and nothing to
/// keep in sync with a bundler.
pub const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>girder</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 14px/1.5 ui-monospace, monospace; margin: 2rem; max-width: 60rem; }
  h1 { font-size: 1.1rem; margin: 0 0 1rem; }
  button { font: inherit; padding: .25rem .6rem; margin-right: .5rem; }
  pre { background: rgba(127,127,127,.12); padding: .75rem; overflow-x: auto; border-radius: 4px; }
  .row { margin: .75rem 0; }
  .error { color: #c0392b; }
  .ok { color: #27ae60; }
</style>
</head>
<body>
<h1>girder — Remote method probe</h1>

<p>Each button calls one Remote method over <code>POST /api/remote.invoke</code> and prints the raw
reply. This page exists to prove the chain end to end, not to be the product.</p>

<div class="row">
  <button data-call="session.projections">session.projections</button>
  <button data-call="job.list">job.list</button>
  <button data-call="session.list">session.list</button>
</div>

<div class="row">
  <label>job id to kill <input id="jobId" size="12" placeholder="job-1"></label>
  <button data-call="job.kill">job.kill</button>
</div>

<h2 style="font-size:.95rem;margin-top:1.5rem">Reply</h2>
<pre id="out">(nothing yet)</pre>

<script>
const out = document.getElementById('out');

/// Calls one Remote method and prints the reply.
///
/// The arguments are built here rather than fetched, so a missing field shows up immediately instead
/// of after a schema round trip.
async function call(namespace, method, args) {
  out.textContent = 'calling ' + namespace + '.' + method + ' …';
  out.className = '';
  try {
    const response = await fetch('/api/remote.invoke', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ namespace, method, args, peer: 'web' }),
    });
    const text = await response.text();
    let pretty = text;
    try { pretty = JSON.stringify(JSON.parse(text), null, 2); } catch (e) { /* keep raw */ }
    out.textContent = 'HTTP ' + response.status + '\n\n' + pretty;
    out.className = response.ok ? 'ok' : 'error';
  } catch (error) {
    out.textContent = 'request failed: ' + error;
    out.className = 'error';
  }
}

for (const button of document.querySelectorAll('button[data-call]')) {
  button.addEventListener('click', () => {
    const [namespace, method] = button.dataset.call.split('.');
    let args = {};
    if (method === 'page') args = { sessionId: 'demo', limit: 10 };
    if (method === 'kill') args = { jobId: document.getElementById('jobId').value };
    call(namespace, method, args);
  });
}
</script>
</body>
</html>
"#;
