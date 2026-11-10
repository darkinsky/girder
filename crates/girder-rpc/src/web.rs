//! The smallest usable browser client, served by the same process.
//!
//! Not the Web UI. It is a working conversation surface: create a session, send a prompt, and watch
//! the answer arrive token by token over the same WebSocket the mux uses. Building it before a real
//! front end is deliberate — a page that uses the API exposes its problems, and those are cheaper to
//! find here than after a build system is in the way.
//!
//! # Why streaming is the point
//!
//! The earlier version of this page only called unary methods and printed a result. A conversation is
//! not a unary call: the answer arrives over time, and a client that showed it at the end would be
//! indistinguishable from one that took thirty seconds to respond. The `agent/turn` subscription is
//! what makes it feel alive, and it is also the only way to see whether the event path works.
//!
//! # What it deliberately does not do
//!
//! No authentication, no reconnect, no history, no markdown rendering. Each is a real need and each
//! would turn this into a different project. The page answers one question: can a browser talk to this
//! server and watch it think?

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
  :root { color-scheme: light dark; --muted: #888; }
  body { font: 14px/1.6 ui-sans-serif, system-ui, sans-serif; margin: 0; padding: 1.5rem 2rem 4rem; max-width: 52rem; }
  h1 { font-size: 1rem; font-weight: 600; margin: 0 0 .25rem; }
  .sub { color: var(--muted); font-size: .8rem; margin-bottom: 1.25rem; }
  textarea { width: 100%; box-sizing: border-box; font: inherit; padding: .6rem; min-height: 4.5rem;
             border: 1px solid color-mix(in srgb, currentColor 20%, transparent); border-radius: 6px;
             background: transparent; color: inherit; resize: vertical; }
  button { font: inherit; padding: .4rem .9rem; border-radius: 6px; cursor: pointer;
           border: 1px solid color-mix(in srgb, currentColor 25%, transparent); background: transparent;
           color: inherit; }
  button:hover:not(:disabled) { background: color-mix(in srgb, currentColor 8%, transparent); }
  button:disabled { opacity: .45; cursor: default; }
  .row { display: flex; gap: .5rem; align-items: center; margin-top: .6rem; }
  .status { font-size: .78rem; color: var(--muted); margin-left: auto; }
  .status.live { color: #27ae60; }
  .status.bad { color: #c0392b; }
  #log { margin-top: 1.5rem; display: flex; flex-direction: column; gap: .9rem; }
  .msg { border-left: 2px solid color-mix(in srgb, currentColor 20%, transparent); padding: .1rem 0 .1rem .75rem; }
  .msg .who { font-size: .72rem; text-transform: uppercase; letter-spacing: .06em; color: var(--muted); }
  .msg .body { white-space: pre-wrap; word-break: break-word; }
  .msg.user { border-left-color: #4a90d9; }
  .msg.model { border-left-color: #27ae60; }
  .msg.tool { border-left-color: #d99a27; }
  .msg.err { border-left-color: #c0392b; }
  .cursor::after { content: "▌"; animation: blink 1s steps(2) infinite; }
  @keyframes blink { 50% { opacity: 0; } }
</style>
</head>
<body>
<h1>girder</h1>
<div class="sub">A conversation surface over the Remote API. Answers stream from the model's own tokens.</div>

<textarea id="prompt" placeholder="Ask something, or ask for a tool: use bash to run echo hi"></textarea>
<div class="row">
  <button id="send">Send</button>
  <button id="stop">Stop</button>
  <span class="status" id="status">connecting…</span>
</div>

<div id="log"></div>

<script>
const log = document.getElementById('log');
const statusEl = document.getElementById('status');
const promptEl = document.getElementById('prompt');
const sendBtn = document.getElementById('send');
const stopBtn = document.getElementById('stop');

let socket = null;
let sessionId = null;
let current = null;   // the assistant row receiving deltas

// ---- small helpers ----------------------------------------------------------------------------

/// Sets the connection indicator.
///
/// A page that looks connected while the socket is closed is worse than one that says it is
/// closed: the user types into nothing.
function setStatus(text, kind) {
  statusEl.textContent = text;
  statusEl.className = 'status' + (kind ? ' ' + kind : '');
}

/// Appends one row and returns the element that receives its body.
function addRow(kind, who) {
  const row = document.createElement('div');
  row.className = 'msg ' + kind;
  const label = document.createElement('div');
  label.className = 'who';
  label.textContent = who;
  const body = document.createElement('div');
  body.className = 'body';
  row.append(label, body);
  log.append(row);
  return body;
}

/// Calls one Remote method over HTTP.
async function call(namespace, method, args) {
  const response = await fetch('/api/remote.invoke', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ namespace, method, args, peer: 'web' }),
  });
  const body = await response.json();
  if (body.status !== 'ok') {
    throw new Error(body.error?.message ?? 'call failed');
  }
  return body.value;
}

// ---- the stream -------------------------------------------------------------------------------

/// Connects the event socket.
///
/// The host pushes `agent/turn` frames as the model produces them; without this the answer only
/// arrives in one piece when the turn ends, which looks like a slow server rather than a fast one.
function connect() {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  socket = new WebSocket(scheme + '//' + location.host + '/api/remote.mux');

  socket.onopen = () => setStatus('connected', 'live');
  socket.onclose = () => {
    setStatus('disconnected', 'bad');
    sendBtn.disabled = true;
    setTimeout(connect, 1000);
  };
  socket.onerror = () => setStatus('socket error', 'bad');

  socket.onmessage = (message) => {
    let frame;
    try { frame = JSON.parse(message.data); } catch { return; }
    if (frame.type !== 'emit' || frame.event !== 'agent/turn') return;
    handleTurn(frame.args?.[0]);
  };
}

/// Applies one `agent/turn` event to the transcript.
function handleTurn(event) {
  if (!event) return;
  switch (event.type) {
    case 'textDelta': {
      // The common case: append to the row that is currently streaming.
      if (!current) current = addRow('model', 'assistant');
      current.textContent += event.text;
      break;
    }
    case 'toolStarted': {
      current = null;
      addRow('tool', 'tool · ' + event.name).textContent = 'running…';
      break;
    }
    case 'toolFinished': {
      addRow('tool', 'tool · ' + event.name).textContent =
        event.isError ? 'failed' : 'done';
      break;
    }
    case 'turnEnded': {
      current = null;
      break;
    }
    default:
      break;
  }
}

// ---- sending ----------------------------------------------------------------------------------

/// Sends the prompt and streams the answer.
async function send() {
  const text = promptEl.value.trim();
  if (!text) return;

  sendBtn.disabled = true;
  stopBtn.disabled = false;
  promptEl.value = '';
  current = null;
  addRow('user', 'you').textContent = text;

  try {
    if (!sessionId) {
      const created = await call('session', 'create', {});
      sessionId = created.sessionId;
    }
    await call('session', 'prompt', { sessionId, text });
    setStatus('waiting…');
  } catch (error) {
    addRow('err', 'error').textContent = String(error.message ?? error);
    sendBtn.disabled = false;
    stopBtn.disabled = true;
    setStatus('error', 'bad');
  }
}

/// Asks the host to cancel the running turn.
async function stop() {
  if (!sessionId) return;
  try {
    await call('session', 'cancel', { sessionId });
    setStatus('cancelled');
  } catch (error) {
    setStatus(String(error.message ?? error), 'bad');
  } finally {
    sendBtn.disabled = false;
    stopBtn.disabled = true;
  }
}

// The turn ends when the host stops sending; the button state follows that rather than a timer.
const originalHandleTurn = handleTurn;
handleTurn = function (event) {
  originalHandleTurn(event);
  if (event && event.type === 'turnEnded') {
    sendBtn.disabled = false;
    stopBtn.disabled = true;
    setStatus('idle', 'live');
  }
};

sendBtn.addEventListener('click', send);
stopBtn.addEventListener('click', stop);
stopBtn.disabled = true;
promptEl.addEventListener('keydown', (event) => {
  // Cmd/Ctrl+Enter sends, so a newline stays available for a multi-line prompt.
  if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') send();
});

connect();
</script>
</body>
</html>
"#;
