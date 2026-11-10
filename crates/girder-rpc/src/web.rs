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
  body { font: 14px/1.6 ui-sans-serif, system-ui, sans-serif; margin: 0; display: flex; min-height: 100vh; }
  #side { width: 15rem; flex: 0 0 15rem; border-right: 1px solid color-mix(in srgb, currentColor 15%, transparent);
          padding: 1.25rem .9rem; box-sizing: border-box; }
  #main { flex: 1; padding: 1.25rem 2rem 4rem; max-width: 52rem; box-sizing: border-box; }
  #sessions { list-style: none; margin: .75rem 0 0; padding: 0; display: flex; flex-direction: column; gap: .15rem; }
  #sessions li { padding: .35rem .5rem; border-radius: 5px; cursor: pointer; font-size: .82rem;
                 white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  #sessions li:hover { background: color-mix(in srgb, currentColor 7%, transparent); }
  #sessions li.on { background: color-mix(in srgb, currentColor 13%, transparent); font-weight: 600; }
  #sessions li .meta { display: block; font-size: .7rem; color: var(--muted); font-weight: 400; }
  #sessions li { position: relative; }
  #sessions li .rowtools { position: absolute; right: .25rem; top: .25rem; display: none; gap: .1rem; }
  #sessions li:hover .rowtools { display: flex; }
  #sessions li .rowtools button { padding: 0 .3rem; font-size: .7rem; line-height: 1.4;
                                  border: none; background: color-mix(in srgb, currentColor 12%, transparent); }
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
<aside id="side">
  <div class="sub" style="margin:0 0 .5rem">sessions</div>
  <button id="new" style="width:100%">New</button>
  <ul id="sessions"></ul>
</aside>
<div id="main">
<h1>girder</h1>
<div class="sub">A conversation surface over the Remote API. Answers stream from the model's own tokens.</div>

<textarea id="prompt" placeholder="Ask something, or ask for a tool: use bash to run echo hi"></textarea>
<div class="row">
  <button id="send">Send</button>
  <button id="stop">Stop</button>
  <span class="status" id="status">connecting…</span>
</div>

<div id="log"></div>
</div>

<script>
const log = document.getElementById('log');
const statusEl = document.getElementById('status');
const promptEl = document.getElementById('prompt');
const sendBtn = document.getElementById('send');
const stopBtn = document.getElementById('stop');

let socket = null;
let sessionId = null;
let current = null;   // the assistant row receiving deltas

const sessionsEl = document.getElementById('sessions');
const newBtn = document.getElementById('new');

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

// ---- sessions ---------------------------------------------------------------------------------

/// Reloads the sidebar from the host.
///
/// The list is ordered by `updatedAt` descending, which is what a user expects: the conversation they
/// were just in sits at the top.
async function refreshSessions() {
  let items = [];
  try {
    const value = await call('session', 'list', {});
    items = value.items ?? [];
  } catch (error) {
    setStatus('cannot list sessions', 'bad');
    return;
  }
  sessionsEl.replaceChildren();
  for (const item of items) {
    const li = document.createElement('li');
    li.dataset.id = item.sessionId;
    if (item.sessionId === sessionId) li.classList.add('on');
    const name = document.createElement('span');
    // A session has no stored title yet, so the id's tail is the only stable label.
    name.textContent = item.blank ? 'new session' : item.sessionId.slice(-8);
    const meta = document.createElement('span');
    meta.className = 'meta';
    meta.textContent = new Date(item.updatedAt).toLocaleString();
    li.append(name, meta);
    li.addEventListener('click', () => openSession(item.sessionId));

    // Rename and delete sit on the row rather than in a global toolbar: an action that needs a target
    // selected first is one the user has to think about.
    const tools = document.createElement('span');
    tools.className = 'rowtools';
    const renameBtn = document.createElement('button');
    renameBtn.textContent = '✎';
    renameBtn.title = 'rename';
    renameBtn.addEventListener('click', async (event) => {
      event.stopPropagation();
      const title = window.prompt('Title', item.sessionId.slice(-8));
      if (!title) return;
      try {
        await call('session', 'rename', { sessionId: item.sessionId, title });
        refreshSessions();
      } catch (error) {
        setStatus(String(error.message ?? error), 'bad');
      }
    });
    const deleteBtn = document.createElement('button');
    deleteBtn.textContent = '×';
    deleteBtn.title = 'delete';
    deleteBtn.addEventListener('click', async (event) => {
      event.stopPropagation();
      // A confirmation, because deleting a conversation is not recoverable from this UI.
      if (!window.confirm('Delete this conversation?')) return;
      try {
        await call('session', 'delete', { sessionId: item.sessionId });
        if (sessionId === item.sessionId) {
          sessionId = null;
          log.replaceChildren();
        }
        refreshSessions();
      } catch (error) {
        setStatus(String(error.message ?? error), 'bad');
      }
    });
    tools.append(renameBtn, deleteBtn);
    li.append(tools);

    sessionsEl.append(li);
  }
}

/// Switches to a session and loads its transcript.
async function openSession(id) {
  if (id === sessionId) return;
  sessionId = id;
  current = null;
  log.replaceChildren();

  for (const li of sessionsEl.children) {
    li.classList.toggle('on', li.dataset.id === id);
  }

  // Load what has already happened. Without this a switch would show an empty page and the user
  // would think the conversation was lost.
  try {
    const page = await call('session', 'events', {
      address: { kind: 'session', sessionId: id },
    });
    for (const record of page.records ?? []) {
      renderEvent(record.event);
    }
  } catch (error) {
    addRow('err', 'error').textContent = String(error.message ?? error);
  }
  promptEl.focus();
}

/// Renders one durable event into the transcript.
///
/// The durable log and the live stream carry different shapes: the log holds whole messages, the
/// stream holds deltas. Rendering both through one function keeps a loaded transcript and a live one
/// looking the same.
function renderEvent(event) {
  if (!event) return;
  switch (event.type) {
    case 'user/message':
    case 'assistant/message': {
      const text = (event.message?.content ?? [])
        .filter((block) => block.type === 'text')
        .map((block) => block.text)
        .join('');
      if (!text) return;
      addRow(event.type === 'user/message' ? 'user' : 'model',
             event.type === 'user/message' ? 'you' : 'assistant').textContent = text;
      break;
    }
    case 'tool/call': {
      addRow('tool', 'tool · ' + event.name).textContent = event.arguments ?? '';
      break;
    }
    case 'tool/result': {
      const text = (event.content ?? [])
        .filter((block) => block.type === 'text')
        .map((block) => block.text)
        .join('');
      addRow('tool', 'tool result').textContent = text || '(empty)';
      break;
    }
    default:
      break;
  }
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
    // The list is ordered by activity, so this prompt moved the session to the top.
    refreshSessions();
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

/// Starts a fresh conversation.
///
/// The old one stays in the list: a "new" that discarded the previous conversation would make the
/// button a destructive action with a benign name.
async function startNew() {
  sessionId = null;
  current = null;
  log.replaceChildren();
  promptEl.value = '';
  promptEl.focus();
  for (const li of sessionsEl.children) li.classList.remove('on');
  setStatus('new conversation');
}

newBtn.addEventListener('click', startNew);
sendBtn.addEventListener('click', send);
stopBtn.addEventListener('click', stop);
stopBtn.disabled = true;
promptEl.addEventListener('keydown', (event) => {
  // Cmd/Ctrl+Enter sends, so a newline stays available for a multi-line prompt.
  if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') send();
});

connect();
refreshSessions();
</script>
</body>
</html>
"#;
