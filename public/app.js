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

// ---------------------------------------------------------------- copy / paste / drag and drop from this computer
function remotePaste() {
  if (!rfb) return;
  rfb.focus();
  setTimeout(() => {
    rfb.sendKey(0xffe3, 'ControlLeft', true);
    rfb.sendKey(0x0076, 'KeyV');
    rfb.sendKey(0xffe3, 'ControlLeft', false);
  }, 120);
}
function uploadFile(file, label) {
  // PUT with progress, into the shared Files folder under uploads/
  return new Promise((resolve, reject) => {
    const name = (file.name || 'pasted-file').replace(/[\/\\]/g, '_').replace(/^\.+/, '') || 'file';
    const x = new XMLHttpRequest();
    x.open('PUT', `/api/files/uploads/${encodeURIComponent(name)}`);
    x.upload.onprogress = (e) => { if (e.lengthComputable) status(`${label} ${name}: ${Math.round((e.loaded / e.total) * 100)}%`); };
    x.onload = () => { try { x.status === 200 ? resolve(JSON.parse(x.responseText).path) : reject(new Error(JSON.parse(x.responseText).error)); } catch { reject(new Error('upload failed')); } };
    x.onerror = () => reject(new Error('upload failed'));
    x.send(file);
  });
}
function status(msg) { const t = $('toast'); clearTimeout(toastTimer); t.textContent = msg; t.hidden = false; }
async function uploadAll(files, label) {
  const paths = [];
  for (const f of files) paths.push(await uploadFile(f, label));
  return paths;
}

// Whatever was copied on this computer (text, image, files) goes to the remote clipboard, then Ctrl+V there.
async function sendToRemoteClipboard({ files = [], text = '' }) {
  try {
    if (files.length === 1 && files[0].type === 'image/png' && files[0].size < 30 * 1024 * 1024) {
      const r = await fetch('/api/clipboard', { method: 'POST', headers: { 'Content-Type': 'image/png' }, body: files[0] });
      if (!r.ok) throw new Error((await r.json()).error);
    } else if (files.length) {
      const paths = await uploadAll(files, 'Pasting');
      const r = await post('/api/clipboard', { files: paths });
      if (!r.ok) throw new Error((await r.json()).error);
      toast(`Pasted ${files.length} file${files.length > 1 ? 's' : ''} (also saved in Files → uploads)`);
    } else if (text) {
      const r = await fetch('/api/clipboard', { method: 'POST', headers: { 'Content-Type': 'text/plain; charset=utf-8' }, body: text });
      if (!r.ok) throw new Error((await r.json()).error);
    } else return;
    remotePaste();
  } catch (e) { toast(`Paste failed: ${e.message}`); }
}

// Ctrl+V / Cmd+V while the remote screen has focus: let this browser fire a real "paste" event
// on a hidden box, so we receive text, images and copied files without any permission prompt.
const trap = $('paste-trap');
const screenFocused = () => $('screen').contains(document.activeElement);
window.addEventListener('keydown', (e) => {
  if (!rfb || !(e.ctrlKey || e.metaKey) || e.altKey || e.key.toLowerCase() !== 'v' || !screenFocused()) return;
  e.stopImmediatePropagation();   // noVNC must not swallow it; the default action (paste) still happens
  trap.value = '';
  trap.focus();
}, true);
trap.addEventListener('paste', (e) => {
  e.preventDefault();
  const files = [...e.clipboardData.files];
  const text = e.clipboardData.getData('text/plain');
  if (rfb) rfb.focus();
  sendToRemoteClipboard({ files, text });
});
trap.addEventListener('blur', () => { trap.value = ''; });

// Paste button (for touch devices and when the keyboard shortcut is not available)
$('btn-paste').addEventListener('click', async () => {
  if (!rfb) return;
  try {
    const items = await navigator.clipboard.read();
    const files = []; let text = '';
    for (const it of items) {
      const img = it.types.find((t) => t.startsWith('image/'));
      if (img) { const b = await it.getType(img); files.push(new File([b], `pasted-image.${img.split('/')[1]}`, { type: img })); }
      else if (it.types.includes('text/plain')) text = await (await it.getType('text/plain')).text();
    }
    return sendToRemoteClipboard({ files, text });
  } catch {
    const text = prompt('Paste the text to send into the remote browser:') || '';
    if (text) sendToRemoteClipboard({ text });
  }
});

// Drag files or text from this computer onto the remote browser
const stage = $('stage');
let dragDepth = 0;
const hasPayload = (e) => e.dataTransfer && [...e.dataTransfer.types].some((t) => t === 'Files' || t === 'text/plain' || t === 'text/uri-list');
stage.addEventListener('dragenter', (e) => { if (!hasPayload(e)) return; e.preventDefault(); dragDepth++; $('dropzone').hidden = false; });
stage.addEventListener('dragover', (e) => { if (!hasPayload(e)) return; e.preventDefault(); e.dataTransfer.dropEffect = 'copy'; });
stage.addEventListener('dragleave', () => { dragDepth = Math.max(0, dragDepth - 1); if (!dragDepth) $('dropzone').hidden = true; });
stage.addEventListener('drop', async (e) => {
  if (!hasPayload(e)) return;
  e.preventDefault();
  dragDepth = 0; $('dropzone').hidden = true;
  const files = [...e.dataTransfer.files];
  const text = files.length ? '' : (e.dataTransfer.getData('text/uri-list') || e.dataTransfer.getData('text/plain'));
  // where on the remote screen the drop happened
  const canvas = $('screen').querySelector('canvas');
  const box = (canvas || stage).getBoundingClientRect();
  const x = (e.clientX - box.left) * (canvas ? canvas.width / box.width : 1);
  const y = (e.clientY - box.top) * (canvas ? canvas.height / box.height : 1);
  try {
    const paths = await uploadAll(files, 'Uploading');
    const r = await post('/api/drop', { x, y, files: paths, text });
    if (r.ok) toast(files.length ? `Dropped ${files.length} file${files.length > 1 ? 's' : ''} into the page` : 'Dropped');
    else if (files.length) toast('Saved in Files → uploads. This spot does not accept drops: use the site\'s upload button and pick it from Downloads/uploads.');
    else toast('This spot does not accept drops');
    if (rfb) rfb.focus();
  } catch (err) { toast(`Drop failed: ${err.message}`); }
});

// ---------------------------------------------------------------- state
function renderState() {
  $('pill-browser').classList.toggle('up', !!st.browserUp);
  $('pill-mcp').classList.toggle('up', !!st.mcpUp);
  const b = $('btn-pause');
  b.textContent = st.paused ? 'Resume agent' : 'Pause agent';
  b.classList.toggle('paused', !!st.paused);
  $('banner').hidden = !st.paused;
  const hadHelp = !$('help').hidden;
  $('help').hidden = !st.help;
  if (st.help) {
    $('help-msg').textContent = `${st.help.message} (${st.help.client})`;
    if (!hadHelp) { toast('The agent is asking for your help'); document.title = '(!) Agent Browser'; }
  } else { document.title = 'Agent Browser'; if (hadHelp) $('help-reply').value = ''; }
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
  $('files-url').textContent = st.filesUrl;
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
  es.addEventListener('files', () => { if (!$('tab-files').hidden) loadFiles(); });
  es.addEventListener('tools', loadTools);
  es.addEventListener('cleared', () => { $('log').textContent = ''; $('log-empty').hidden = false; });
  es.onerror = () => { fetch('/api/state').then((r) => { if (r.status === 401) location.href = '/login'; }).catch(() => {}); };
}

// ---------------------------------------------------------------- files
function fmtSize(n) {
  if (n < 1024) return `${n} B`;
  const u = ['KB', 'MB', 'GB']; let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < u.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${u[i]}`;
}
const enc = (p) => p.split('/').map(encodeURIComponent).join('/');

async function loadFiles() {
  const r = await fetch('/api/files');
  if (!r.ok) return;
  const { files } = await r.json();
  const ul = $('files'); ul.textContent = '';
  $('files-empty').hidden = files.length > 0;
  for (const f of files) {
    const li = document.createElement('li');
    const name = document.createElement('div'); name.className = 'name';
    const a = document.createElement('a'); a.href = `/files/${enc(f.path)}`; a.textContent = f.path; a.download = '';
    const meta = document.createElement('div'); meta.className = 'meta';
    meta.textContent = `${fmtSize(f.size)} · ${new Date(f.mtime).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' })}`;
    name.append(a, meta);
    const del = document.createElement('button'); del.className = 'link'; del.textContent = 'Delete';
    del.setAttribute('aria-label', `Delete ${f.path}`);
    del.addEventListener('click', async () => {
      if (!confirm(`Delete ${f.path}?`)) return;
      await fetch(`/api/files/${enc(f.path)}`, { method: 'DELETE' }); loadFiles();
    });
    li.append(name, del); ul.append(li);
  }
}
$('btn-upload').addEventListener('click', () => $('file-input').click());
$('file-input').addEventListener('change', async (e) => {
  const files = [...e.target.files]; e.target.value = '';
  for (const [i, f] of files.entries()) {
    $('upload-status').textContent = `Uploading ${i + 1} of ${files.length}…`;
    const r = await fetch(`/api/files/${encodeURIComponent(f.name)}`, { method: 'PUT', body: f }).catch(() => null);
    if (!r || !r.ok) { toast(`Upload failed: ${f.name}`); break; }
  }
  $('upload-status').textContent = '';
  loadFiles();
});

// ---------------------------------------------------------------- tools made by agents
async function loadTools() {
  const r = await fetch('/api/tools');
  if (!r.ok) return;
  const { tools } = await r.json();
  const ul = $('tools'); ul.textContent = '';
  $('tools-empty').hidden = tools.length > 0;
  for (const t of tools) {
    const li = document.createElement('li');
    const row = document.createElement('div'); row.className = 'row';
    const name = document.createElement('span'); name.className = 'tname'; name.textContent = `x_${t.name}`;
    const del = document.createElement('button'); del.className = 'link'; del.textContent = 'Delete';
    del.setAttribute('aria-label', `Delete tool ${t.name}`);
    del.addEventListener('click', async () => {
      if (!confirm(`Delete the agent tool x_${t.name}?`)) return;
      await fetch(`/api/tools/${encodeURIComponent(t.name)}`, { method: 'DELETE' }); loadTools();
    });
    row.append(name, del);
    const desc = document.createElement('div'); desc.className = 'muted small'; desc.textContent = t.description;
    const det = document.createElement('details');
    const sum = document.createElement('summary'); sum.textContent = 'Show code';
    const pre = document.createElement('pre'); pre.textContent = t.code;
    det.append(sum, pre);
    li.append(row, desc, det); ul.append(li);
  }
}

$('btn-help-done').addEventListener('click', () => post('/api/help-done', { reply: $('help-reply').value }));
$('help-reply').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('btn-help-done').click(); });

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
    for (const name of ['activity', 'files', 'connect']) $('tab-' + name).hidden = tab.dataset.tab !== name;
    if (tab.dataset.tab === 'files') loadFiles();
  });
}
for (const btn of document.querySelectorAll('[data-copy]')) {
  btn.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText($(btn.dataset.copy).textContent); toast('Copied'); }
    catch { toast('Copy failed – select the text and copy it manually'); }
  });
}

await loadState();
loadTools();
listen();
connectScreen();
