import RFB from '/novnc/core/rfb.js';

const $ = (id) => document.getElementById(id);
const post = (url, body = {}) => fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });

let st = {};
let rfb = null;
let retry = null;

// ---------------------------------------------------------------- toast
let toastTimer;
function toast(msg) {
  const t = $('toast'); t.textContent = msg; t.hidden = false;
  clearTimeout(toastTimer); toastTimer = setTimeout(() => { t.hidden = true; }, 2500);
}

// ---------------------------------------------------------------- live view
function overlay(msg) { const o = $('overlay'); o.hidden = !msg; if (msg) o.textContent = msg; }

function connectScreen() {
  clearTimeout(retry);
  if (rfb) return;
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  rfb = new RFB($('screen'), `${proto}://${location.host}/vnc`);
  rfb.resizeSession = true;     // the remote browser resizes to fit this window
  rfb.scaleViewport = false;
  rfb.background = '#010409';
  rfb.addEventListener('connect', () => { overlay(''); rfb.focus(); });
  rfb.addEventListener('disconnect', () => {
    rfb = null;
    overlay('Reconnecting to the browser…');
    retry = setTimeout(connectScreen, 2000);
  });
  // text copied inside the remote browser -> this computer's clipboard
  rfb.addEventListener('clipboard', (e) => {
    if (navigator.clipboard && document.hasFocus()) navigator.clipboard.writeText(e.detail.text).catch(() => {});
  });
}

// this computer's clipboard -> remote browser, then Ctrl+V there
$('btn-paste').addEventListener('click', async () => {
  if (!rfb) return;
  let text = '';
  try { text = await navigator.clipboard.readText(); }
  catch { text = prompt('Paste the text to send into the remote browser:') || ''; }
  if (!text) return;
  rfb.clipboardPasteFrom(text);
  rfb.focus();
  setTimeout(() => {
    rfb.sendKey(0xffe3, 'ControlLeft', true);
    rfb.sendKey(0x0076, 'KeyV');
    rfb.sendKey(0xffe3, 'ControlLeft', false);
  }, 150);
});

// ---------------------------------------------------------------- state
function renderState() {
  $('pill-browser').classList.toggle('up', !!st.browserUp);
  $('pill-mcp').classList.toggle('up', !!st.mcpUp);
  const b = $('btn-pause');
  b.textContent = st.paused ? 'Resume agent' : 'Pause agent';
  b.classList.toggle('paused', !!st.paused);
  $('banner').hidden = !st.paused;
}

function fmtTime(ms) { return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }); }

function addEntry(e, initial = false) {
  const li = document.createElement('li');
  li.className = e.kind;
  const top = document.createElement('div'); top.className = 'top';
  const who = document.createElement('span'); who.textContent = e.client || 'agent';
  const when = document.createElement('span'); when.textContent = fmtTime(e.time);
  top.append(who, when); li.append(top);
  if (e.tool) { const t = document.createElement('div'); t.className = 'tool'; t.textContent = e.tool; li.append(t); }
  if (e.detail) { const d = document.createElement('div'); d.className = 'detail'; d.textContent = e.detail; li.append(d); }
  $('log').prepend(li);
  while ($('log').children.length > 300) $('log').lastChild.remove();
  $('log-empty').hidden = true;
  if (!initial && e.kind === 'connect') toast(`${e.client} connected`);
}

function renderConnect() {
  $('mcp-url').textContent = st.mcpUrl;
  $('mcp-token').textContent = st.mcpToken;
  $('mcp-qurl').textContent = `${st.mcpUrl}?token=${encodeURIComponent(st.mcpToken)}`;
  $('mcp-json').textContent = JSON.stringify({
    mcpServers: { 'krish-browser': { type: 'http', url: st.mcpUrl, headers: { Authorization: `Bearer ${st.mcpToken}` } } },
  }, null, 2);
}

async function loadState() {
  const r = await fetch('/api/state');
  if (r.status === 401) { location.href = '/login'; return; }
  st = await r.json();
  renderState(); renderConnect();
  $('log').textContent = ''; $('log-empty').hidden = false;
  for (const e of st.activity) addEntry(e, true);
}

function listen() {
  const es = new EventSource('/api/events');
  es.addEventListener('state', (e) => { Object.assign(st, JSON.parse(e.data)); renderState(); });
  es.addEventListener('activity', (e) => addEntry(JSON.parse(e.data)));
  es.addEventListener('cleared', () => { $('log').textContent = ''; $('log-empty').hidden = false; });
  es.onerror = () => { fetch('/api/state').then((r) => { if (r.status === 401) location.href = '/login'; }).catch(() => {}); };
}

// ---------------------------------------------------------------- controls
$('btn-pause').addEventListener('click', () => post('/api/pause', { paused: !st.paused }));
$('btn-clear').addEventListener('click', () => post('/api/clear-activity'));
$('btn-restart').addEventListener('click', async () => {
  if (!confirm('Restart the browser? Open tabs are restored, but anything half-typed is lost and agents must reconnect.')) return;
  await post('/api/restart-browser'); toast('Browser is restarting…');
});
$('btn-logout').addEventListener('click', async () => { await post('/logout'); location.href = '/login'; });
$('btn-show').addEventListener('click', () => {
  const shown = $('mcp-token').classList.toggle('shown');
  $('btn-show').textContent = shown ? 'Hide' : 'Show';
});
$('btn-panel').addEventListener('click', () => {
  const hidden = document.body.classList.toggle('no-panel');
  $('btn-panel').setAttribute('aria-expanded', String(!hidden));
  try { localStorage.setItem('kab-panel', hidden ? '0' : '1'); } catch {}
});
try { if (localStorage.getItem('kab-panel') === '0') { document.body.classList.add('no-panel'); $('btn-panel').setAttribute('aria-expanded', 'false'); } } catch {}

for (const tab of document.querySelectorAll('.tabs button')) {
  tab.addEventListener('click', () => {
    for (const t of document.querySelectorAll('.tabs button')) t.setAttribute('aria-selected', String(t === tab));
    $('tab-activity').hidden = tab.dataset.tab !== 'activity';
    $('tab-connect').hidden = tab.dataset.tab !== 'connect';
  });
}
for (const btn of document.querySelectorAll('[data-copy]')) {
  btn.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText($(btn.dataset.copy).textContent); toast('Copied'); }
    catch { toast('Copy failed – select the text and copy it manually'); }
  });
}

await loadState();
listen();
connectScreen();
