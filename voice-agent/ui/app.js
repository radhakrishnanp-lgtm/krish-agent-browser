// Voice Agent UI. One WebSocket to the server:
//   browser -> server: binary = 16 kHz mono Int16 PCM microphone audio; JSON = control messages
//   server -> browser: binary = reply audio (PCM, Int16, mono); JSON = status / transcript / cost / log
'use strict';

const $ = (id) => document.getElementById(id);
const money = (v) => (v == null ? '–' : '$' + Number(v).toFixed(6));
const secs = (ms) => (ms / 1000).toFixed(1) + ' s';
const store = {
  get(k) { try { return localStorage.getItem(k); } catch (e) { return null; } },
  set(k, v) { try { v == null || v === '' ? localStorage.removeItem(k) : localStorage.setItem(k, v); } catch (e) { /* private mode */ } },
};
const STATES = {
  idle: ['Not connected', ''],
  listening: ['Listening', 'Just start talking.'],
  user_speaking: ['Hearing you…', 'Pause when you are done.'],
  transcribing: ['Transcribing…', ''],
  thinking: ['Thinking…', ''],
  speaking: ['Speaking', 'Talk over me to interrupt.'],
};

let ws = null, hello = null, conv = null, convs = [];
let serverState = 'idle', connected = false, retries = 0, retryTimer = null;
let micOn = false, micCtx = null, micStream = null, micLevel = 0;
let playCtx = null, playHead = 0, playing = new Set(), acceptAudio = false, outRate = 24000, playLevel = 0;
let errCount = 0, replyTimes = [];
let replay = null, replayBtn = null;
let testing = null; // microphone test in progress: { chunks, peak }

// ---------- connection ----------

function connect() {
  clearTimeout(retryTimer);
  setConn('wait', retries ? 'Reconnecting…' : 'Connecting…');
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  ws = new WebSocket(`${proto}://${location.host}/ws`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => { connected = true; setConn('on', 'Connected'); hideBanner(); };
  ws.onmessage = (ev) => (typeof ev.data === 'string' ? onMessage(JSON.parse(ev.data)) : onAudio(ev.data));
  ws.onclose = (ev) => {
    const wasConnected = connected;
    connected = false;
    stopPlayback();
    setState('idle');
    if (ev.code === 4401) {
      setConn('off', 'Access token needed');
      login();
      return;
    }
    if (wasConnected) { retries = 0; addLog({ level: 'warn', message: `Connection lost (code ${ev.code}). Reconnecting…` }); }
    if (retries < 6) {
      setConn('wait', 'Reconnecting…');
      retryTimer = setTimeout(connect, Math.min(8000, 500 * 2 ** retries++));
    } else {
      setConn('off', 'Disconnected');
      showBanner('Cannot reach the server. Check that it is running.', 'Try again', () => { retries = 0; connect(); });
    }
  };
}

// The token is exchanged for an HttpOnly cookie, so it is never kept in the page or put in a URL.
async function login(wrong) {
  const t = prompt(wrong ? 'That token was not accepted. Try again:' : 'This server needs an access token (ACCESS_TOKEN in its .env):');
  if (!t) { showBanner('Sign in to use the voice agent.', 'Sign in', () => login()); return; }
  const r = await fetch('/api/login', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ token: t.trim() }) }).catch(() => null);
  if (r && r.ok) { hideBanner(); retries = 0; connect(); } else if (r && r.status === 401) login(true); else showBanner('Could not reach the server to sign in.', 'Try again', () => login());
}

function send(obj) {
  if (ws && ws.readyState === WebSocket.OPEN) { ws.send(JSON.stringify(obj)); return true; }
  showBanner('Not connected to the server, so that was not sent.');
  return false;
}

function setConn(cls, text) {
  $('connDot').className = 'dot ' + cls;
  $('connText').textContent = text;
}

// ---------- server messages ----------

function onMessage(m) {
  switch (m.type) {
    case 'hello': onHello(m); break;
    case 'conversations': convs = m.items; renderConvList(); break;
    case 'conversation': showConversation(m.conversation); break;
    case 'conversation_meta':
      if (conv && conv.id === m.id) { conv.title = m.title; renderHeader(); }
      break;
    case 'status': setState(m.state); break;
    case 'transcript': onTranscript(m); break;
    case 'cost': showCost(m); break;
    case 'log': addLog(m); break;
    case 'raw': addLog({ level: 'raw', ts: m.ts, message: `Raw ${m.stage.toUpperCase()} response`, data: m.payload }); break;
    case 'error': addRow('error', null, m.message); break;
    case 'audio_start': acceptAudio = true; break;
    case 'interrupt': stopPlayback(); break;
    case 'review': showReview(m); break;
    case 'instructions':
      hello.instructions = m.global;
      if (conv) conv.instructions = m.conversation;
      renderHeader();
      break;
  }
}

function onHello(m) {
  const first = !hello || hello.session_id !== m.session_id;
  hello = m; outRate = m.output_sample_rate;
  $('mLlm').textContent = m.models.llm; $('mStt').textContent = m.models.stt; $('mTts').textContent = m.models.tts;
  $('sid').textContent = m.session_id;
  fillSelect($('preset'), m.presets.map((p) => [p, p]), m.preset);
  const savedVoice = store.get('voiceAgentVoice');
  const voice = m.voices.includes(savedVoice) ? savedVoice : '';
  fillSelect($('voice'), [['', `Default (${m.default_voice || 'model default'})`], ...m.voices.map((v) => [v, v])], voice);
  $('speed').value = store.get('voiceAgentSpeed') || '';
  $('pause').value = store.get('voiceAgentPause') || '';
  $('sensitivity').value = store.get('voiceAgentSensitivity') || '';
  $('recordToggle').disabled = !m.recording_allowed;
  $('recordToggle').checked = m.recording_allowed && store.get('voiceAgentRecord') !== 'off';
  sendSettings();
  $('modeCards').innerHTML = '';
  for (const mode of m.modes) {
    const b = el('div', null, 'modecard');
    b.tabIndex = 0; b.setAttribute('role', 'button');
    b.append(el('b', mode.name), el('span', mode.description));
    const start = () => { ensurePlayCtx(); send({ type: 'new', mode: mode.id }); };
    b.onclick = start;
    b.onkeydown = (e) => { if (e.target === b && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); start(); } };
    if (mode.custom) {
      const del = el('button', '✕', 'del'); del.title = 'Delete this mode'; del.setAttribute('aria-label', `Delete mode ${mode.name}`);
      del.onclick = (e) => { e.stopPropagation(); if (confirm(`Delete the mode "${mode.name}"? Conversations that used it are kept.`)) send({ type: 'delete_mode', id: mode.id }); };
      b.append(del);
    }
    $('modeCards').append(b);
  }
  const add = el('button', null, 'modecard add');
  add.append(el('b', '+ Create your own'), el('span', 'Interview practice, a language tutor, a storyteller: describe it and start talking.'));
  add.onclick = () => $('modeDlg').showModal();
  $('modeCards').append(add);
  if (conv) renderHeader();
  if (first) { // new connection: go back to the conversation that was open
    const last = conv ? conv.id : store.get('voiceAgentConv');
    if (last) send({ type: 'open', id: last }); else showConversation(null);
  }
}

function fillSelect(sel, options, value) {
  sel.innerHTML = '';
  for (const [v, label] of options) sel.add(new Option(label, v, false, v === value));
}

function el(tag, text, cls) {
  const e = document.createElement(tag);
  if (text != null) e.textContent = text;
  if (cls) e.className = cls;
  return e;
}

function sendSettings() {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({ type: 'settings', voice: $('voice').value || null, speed: $('speed').value || null,
      pause_ms: $('pause').value || null, sensitivity: $('sensitivity').value || null, record: $('recordToggle').checked }));
  }
}

// ---------- conversations ----------

const modeName = (id) => (hello?.modes.find((m) => m.id === id) || {}).name || id;

function ago(iso) {
  const s = (Date.now() - new Date(iso).getTime()) / 1000;
  if (!isFinite(s)) return '';
  if (s < 60) return 'just now';
  if (s < 3600) return Math.floor(s / 60) + ' min ago';
  if (s < 86400) return Math.floor(s / 3600) + ' h ago';
  if (s < 86400 * 7) return Math.floor(s / 86400) + ' d ago';
  return new Date(iso).toLocaleDateString();
}

function renderConvList() {
  const box = $('convList');
  box.innerHTML = '';
  const q = $('search').value.trim().toLowerCase();
  const shown = q ? convs.filter((c) => c.title.toLowerCase().includes(q) || modeName(c.mode).toLowerCase().includes(q)) : convs;
  if (!shown.length) { box.append(el('div', q ? 'No conversation matches.' : 'Nothing saved yet.', 'none')); return; }
  for (const c of shown) {
    const row = el('div', null, 'conv' + (conv && conv.id === c.id ? ' on' : ''));
    row.tabIndex = 0;
    row.setAttribute('role', 'button');
    const open = () => { closeSide(); if (!conv || conv.id !== c.id) send({ type: 'open', id: c.id }); };
    row.onclick = open;
    row.onkeydown = (e) => { if (e.target === row && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); open(); } };
    const acts = el('span', null, 'acts');
    const ren = el('button', '✎'); ren.title = 'Rename'; ren.setAttribute('aria-label', `Rename ${c.title}`);
    ren.onclick = (e) => { e.stopPropagation(); const t = prompt('Rename conversation:', c.title); if (t && t.trim()) send({ type: 'rename', id: c.id, title: t }); };
    const del = el('button', '✕'); del.title = 'Delete'; del.setAttribute('aria-label', `Delete ${c.title}`);
    del.onclick = (e) => { e.stopPropagation(); if (confirm(`Delete "${c.title}"? This cannot be undone.`)) send({ type: 'delete', id: c.id }); };
    acts.append(ren, del);
    row.append(el('span', c.title, 't'), acts, el('span', `${modeName(c.mode)} · ${ago(c.updated)} · ${c.turns} turn${c.turns === 1 ? '' : 's'}`, 's'));
    box.append(row);
  }
}

function showConversation(c) {
  stopPlayback();
  stopReplay();
  conv = c;
  showSpoken(c ? c.user_seconds : 0);
  store.set('voiceAgentConv', c ? c.id : null);
  $('welcome').hidden = !!c;
  $('transcript').hidden = !c;
  $('transcript').innerHTML = '';
  if (c) {
    for (const m of c.messages) addRow(m.role, null, m.content, m);
    showConvCost(c.totals);
    if (!c.messages.length) $('transcript').append(el('div', 'Say something, or type below.', 'daybreak'));
  } else {
    showConvCost(null);
  }
  renderHeader();
  renderConvList();
  scrollDown(true);
}

function renderHeader() {
  $('convTitle').textContent = conv ? conv.title : 'Voice Agent';
  $('modeBadge').hidden = !conv;
  if (conv) $('modeBadge').textContent = modeName(conv.mode);
  $('reviewBtn').hidden = !(conv && hello?.modes.find((m) => m.id === conv.mode)?.review);
  $('instrBtn').classList.toggle('set', !!(hello?.instructions || conv?.instructions));
  document.title = conv ? `${conv.title} · Voice Agent` : 'Voice Agent';
}

// ---------- transcript ----------

function scrollDown(force) {
  const box = $('transcript');
  if (force || box.scrollHeight - box.scrollTop - box.clientHeight < 160) box.scrollTop = box.scrollHeight;
}

// In English practice the tutor starts a correction with: You can say: "..."
const FIX = /^\s*You (?:can|could) say:?\s*["“]([^"”]+)["”]\.?\s*/i;

function fillBubble(bubble, role, text) {
  bubble.textContent = '';
  const m = role === 'assistant' && FIX.exec(text);
  if (m) {
    const fix = el('span', null, 'fix');
    fix.append(el('small', 'Better way to say it'), document.createTextNode(m[1]));
    bubble.append(fix, document.createTextNode(text.slice(m[0].length)));
  } else {
    bubble.textContent = text;
  }
}

function addRow(role, id, text, info) {
  const box = $('transcript');
  box.querySelector('.daybreak')?.remove();
  let row = id && document.getElementById(id);
  if (!row) {
    row = el('div', null, 'row ' + role);
    if (id) row.id = id;
    row.append(el('div', null, 'bubble'));
    box.append(row);
  }
  fillBubble(row.firstChild, role, text);
  if (info) {
    const bits = [];
    if (role === 'assistant') {
      if (info.interrupted) bits.push('interrupted');
      if (info.response_ms) bits.push(`replied in ${secs(info.response_ms)}`);
      if (info.cost) bits.push(money(info.cost));
    }
    if (bits.length || info.audio) {
      let meta = row.querySelector('.meta');
      if (!meta) { meta = el('div', null, 'meta'); row.append(meta); }
      meta.textContent = '';
      if (info.audio) meta.append(playButton(info.audio, role));
      meta.append(document.createTextNode(bits.join(' · ')));
    }
  }
  scrollDown(role === 'user');
  return row;
}

// ---------- replaying recordings ----------

function playButton(name, role) {
  const b = el('button', '▶ Play', 'play');
  b.setAttribute('aria-label', role === 'user' ? 'Play what you said' : 'Play this reply');
  b.onclick = () => {
    const mine = replayBtn === b;
    stopReplay();
    if (mine) return;
    stopPlayback();
    send({ type: 'stop' });
    replay = new Audio(`/api/audio/${conv.id}/${name}`);
    if (replay.setSinkId && $('spkDevice').value) replay.setSinkId($('spkDevice').value).catch(() => {});
    replayBtn = b;
    b.textContent = '■ Stop'; b.classList.add('on');
    replay.onended = stopReplay;
    replay.onerror = () => { stopReplay(); showBanner('That recording could not be played.'); };
    replay.play().catch(() => stopReplay());
  };
  return b;
}

function stopReplay() {
  if (replay) { replay.onended = replay.onerror = null; replay.pause(); replay = null; }
  if (replayBtn) { replayBtn.textContent = '▶ Play'; replayBtn.classList.remove('on'); replayBtn = null; }
}

function showSpoken(seconds) {
  if (conv) conv.user_seconds = seconds;
  const s = Math.round(seconds || 0);
  $('spoken').textContent = s ? `You have spoken for ${Math.floor(s / 60)} min ${s % 60} s in this conversation.`
    : 'You have not spoken in this conversation yet.';
}

// ---------- feedback report ----------

function showReview(m) {
  const body = $('reviewBody');
  $('reviewAgain').disabled = !!m.working;
  if (m.working) { body.textContent = ''; body.append(el('p', 'Reading your conversation…', 'muted')); $('reviewMeta').textContent = ''; return; }
  if (m.error) { body.textContent = ''; body.append(el('p', m.error)); return; }
  const r = m.review;
  if (conv) conv.review = r;
  body.textContent = '';
  if (r.text) { const pre = el('p', r.text); pre.style.whiteSpace = 'pre-wrap'; body.append(pre); }
  if (r.summary) body.append(el('p', r.summary));
  if (r.level) body.append(el('span', `Level: ${r.level}`, 'level'));
  const list = (title, items) => {
    if (!Array.isArray(items) || !items.length) return;
    const ul = el('ul');
    for (const i of items) ul.append(el('li', String(i)));
    body.append(el('h3', title), ul);
  };
  list('What you did well', r.strengths);
  if (Array.isArray(r.mistakes)) {
    body.append(el('h3', r.mistakes.length ? 'Things to fix' : 'No mistakes found'));
    for (const x of r.mistakes) {
      const card = el('div', null, 'mistake');
      card.append(el('div', String(x.said ?? ''), 'said'), el('div', String(x.better ?? ''), 'better'), el('div', String(x.why ?? ''), 'why'));
      body.append(card);
    }
  }
  list('Practise next', r.tips);
  $('reviewMeta').textContent = `${new Date(r.ts).toLocaleString()} · ${r.messages_reviewed} of your messages · ${money(r.cost)}`;
}

function onTranscript(m) {
  if (!m.text || !conv) return;
  if (m.final && m.user_seconds != null) showSpoken(m.user_seconds);
  addRow(m.role, `t-${hello.session_id}-${m.role}-${m.turn}`, m.text, m.final ? m : null);
  if (m.final && m.role === 'assistant' && m.response_ms) {
    replyTimes.push(m.response_ms);
    const avg = replyTimes.reduce((a, b) => a + b, 0) / replyTimes.length;
    $('latency').textContent = `Last reply started after ${secs(m.response_ms)}. Average over ${replyTimes.length}: ${secs(avg)}. ` +
      'Measured from when you stop talking to the first sound.';
  }
}

// ---------- cost ----------

function showConvCost(t) {
  $('costChip').textContent = money(t ? t.total : 0);
  $('cLlm').textContent = money(t?.llm); $('cStt').textContent = money(t?.stt);
  $('cTts').textContent = money(t?.tts); $('cTot').textContent = money(t?.total);
}

function showCost(m) {
  const t = m.turn;
  showConvCost(m.conversation);
  $('sTot').textContent = money(m.session.total);
  if (t) {
    $('tLlm').textContent = money(t.llm_cost); $('tStt').textContent = money(t.stt_cost);
    $('tTts').textContent = money(t.tts_cost); $('tTot').textContent = money(t.turn_total);
    $('usage').textContent = `Last turn: ${t.llm_input_tokens} in / ${t.llm_output_tokens} out tokens, ` +
      `${t.stt_seconds.toFixed(1)} s of your audio, ${t.tts_characters} characters spoken.`;
  }
  if (m.budget_usd > 0) {
    const used = Math.min(1, m.session.total / m.budget_usd);
    $('budgetWrap').hidden = false;
    $('budgetBar').style.width = (used * 100).toFixed(1) + '%';
    $('budgetBar').className = used > 0.8 ? 'high' : '';
    $('budgetText').textContent = `Budget for this visit: ${money(Math.max(0, m.budget_usd - m.session.total))} left of $${m.budget_usd.toFixed(2)}.`;
  }
}

// ---------- log panel ----------

function addLog(m) {
  const box = $('log');
  const stick = box.scrollTop + box.clientHeight >= box.scrollHeight - 30;
  const line = el('div', null, 'line ' + m.level);
  line.append(el('span', new Date(m.ts || Date.now()).toLocaleTimeString([], { hour12: false }) + ' ', 'ts'),
    el('span', m.level.toUpperCase(), 'lvl'), document.createTextNode(' ' + m.message));
  if (m.data !== undefined) {
    const d = el('details');
    d.append(el('summary', 'details'), el('pre', JSON.stringify(m.data, null, 2)));
    line.append(d);
  }
  box.append(line);
  while (box.childElementCount > 1000) box.firstChild.remove();
  if (stick) box.scrollTop = box.scrollHeight;
  if (m.level === 'error') {
    showBanner(m.message, 'Open logs', () => openDetails('logs'));
    if ($('tab-logs').hidden || $('details').classList.contains('closed')) {
      $('errBadge').hidden = false;
      $('errBadge').textContent = ++errCount;
    }
  }
}

function showBanner(text, actionLabel, action) {
  const b = $('banner');
  b.innerHTML = '';
  b.append(el('span', text));
  if (action) { const a = el('button', actionLabel, 'btn small'); a.onclick = action; b.append(a); }
  const x = el('button', 'Dismiss', 'btn small'); x.onclick = hideBanner; b.append(x);
  b.hidden = false;
}
function hideBanner() { $('banner').hidden = true; }

// ---------- state ----------

function setState(s) { serverState = s; renderState(); }

function renderState() {
  // Audio can still be playing after the server has finished the turn.
  const s = playing.size && serverState === 'listening' ? 'speaking' : serverState;
  let [label, hint] = STATES[s] || [s, ''];
  if (s === 'listening' && !micOn) { label = 'Ready'; hint = 'Tap the round button to talk, or type below.'; }
  if (s === 'speaking' && (!micOn || (hello && !hello.barge_in))) hint = '';
  $('micBtn').className = 'mic ' + (s === 'listening' && !micOn ? '' : s);
  $('micBtn').setAttribute('aria-label', micOn ? 'Stop microphone' : 'Start microphone');
  $('icoMic').toggleAttribute('hidden', micOn);
  $('icoStop').toggleAttribute('hidden', !micOn);
  $('micBtn').title = micOn ? 'Stop microphone' : 'Start microphone';
  $('state').textContent = label;
  $('hint').textContent = hint;
  $('stopBtn').hidden = !(s === 'speaking' || s === 'thinking');
}

// ---------- playback ----------

function ensurePlayCtx() {
  if (!playCtx) { playCtx = new (window.AudioContext || window.webkitAudioContext)(); applySpeaker(); }
  if (playCtx.state === 'suspended') playCtx.resume();
}

function applySpeaker() {
  const id = $('spkDevice').value;
  if (playCtx && playCtx.setSinkId) playCtx.setSinkId(id).catch(() => addLog({ level: 'warn', message: 'Could not switch to that speaker; using the default.' }));
}

function onAudio(buf) {
  if (!acceptAudio) return; // stale audio from an interrupted reply
  stopReplay();
  ensurePlayCtx();
  const pcm = new Int16Array(buf);
  const audio = playCtx.createBuffer(1, pcm.length, outRate);
  const ch = audio.getChannelData(0);
  let peak = 0;
  for (let i = 0; i < pcm.length; i++) { ch[i] = pcm[i] / 32768; const a = Math.abs(ch[i]); if (a > peak) peak = a; }
  const src = playCtx.createBufferSource();
  src.buffer = audio; src.connect(playCtx.destination);
  playHead = Math.max(playHead, playCtx.currentTime + 0.04);
  const startsIn = (playHead - playCtx.currentTime) * 1000;
  src.start(playHead);
  playHead += audio.duration;
  playing.add(src);
  setTimeout(() => { if (playing.has(src)) playLevel = peak; }, startsIn);
  src.onended = () => { playing.delete(src); if (!playing.size) playLevel = 0; renderState(); };
  renderState();
}

function stopPlayback() {
  acceptAudio = false;
  for (const src of playing) { try { src.onended = null; src.stop(); } catch (e) { /* already stopped */ } }
  playing.clear(); playHead = 0; playLevel = 0;
  renderState();
}

// ---------- microphone ----------

async function startMic() {
  if (!navigator.mediaDevices?.getUserMedia) {
    showBanner('This browser cannot use the microphone on this address. Open the page over https:// or on http://localhost.');
    return;
  }
  try {
    ensurePlayCtx();
    const base = { channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true };
    const wanted = store.get('voiceAgentMic');
    try {
      micStream = await navigator.mediaDevices.getUserMedia({ audio: wanted ? { ...base, deviceId: { exact: wanted } } : base });
    } catch (e) {
      if (!wanted || (e.name !== 'OverconstrainedError' && e.name !== 'NotFoundError')) throw e;
      store.set('voiceAgentMic', null); // the saved microphone is unplugged: fall back to the default
      addLog({ level: 'warn', message: 'Your chosen microphone is not connected; using the default one.' });
      micStream = await navigator.mediaDevices.getUserMedia({ audio: base });
    }
    micCtx = new (window.AudioContext || window.webkitAudioContext)();
    await micCtx.audioWorklet.addModule('mic-worklet.js');
    const node = new AudioWorkletNode(micCtx, 'mic-processor');
    node.port.onmessage = (ev) => {
      micLevel = ev.data.peak;
      if (testing) { // microphone test: keep the audio here, send nothing
        if (testing.chunks) { testing.chunks.push(new Int16Array(ev.data.pcm)); testing.peak = Math.max(testing.peak, ev.data.peak); }
        return;
      }
      // With barge-in off, do not send the assistant's own voice back to the server.
      if (hello && !hello.barge_in && playing.size) return;
      if (replay) return; // a replayed recording must not be heard as a new turn
      if (ws && ws.readyState === WebSocket.OPEN) ws.send(ev.data.pcm);
    };
    micCtx.createMediaStreamSource(micStream).connect(node);
    micOn = true;
    const label = micStream.getAudioTracks()[0]?.label || 'default';
    addLog({ level: 'info', message: `Microphone on: ${label} (${micCtx.sampleRate} Hz, sent as 16000 Hz)` });
    micStream.getAudioTracks()[0].onended = () => { stopMic(); showBanner('The microphone was disconnected.'); };
    refreshDevices();
  } catch (e) {
    const why = e.name === 'NotAllowedError' ? 'permission was denied. Allow the microphone for this site and try again.'
      : e.name === 'NotFoundError' ? 'no microphone was found.' : e.message;
    stopMic();
    addLog({ level: 'error', message: `Microphone failed: ${why}` });
  }
  renderState();
}

function stopMic() {
  micStream?.getTracks().forEach((t) => t.stop());
  micCtx?.close();
  micStream = micCtx = null; micOn = false; micLevel = 0;
  renderState();
}

// ---------- devices and microphone test ----------

async function refreshDevices() {
  if (!navigator.mediaDevices?.enumerateDevices) return;
  const devices = await navigator.mediaDevices.enumerateDevices().catch(() => []);
  const fill = (sel, kind, saved, word) => {
    const list = devices.filter((d) => d.kind === kind && d.deviceId && d.deviceId !== 'default' && d.deviceId !== 'communications');
    const opts = [['', 'System default'], ...list.map((d, i) => [d.deviceId, d.label || `${word} ${i + 1}`])];
    fillSelect(sel, opts, list.some((d) => d.deviceId === saved) ? saved : '');
  };
  fill($('micDevice'), 'audioinput', store.get('voiceAgentMic') || '', 'Microphone');
  fill($('spkDevice'), 'audiooutput', store.get('voiceAgentSpk') || '', 'Speaker');
  $('spkDevice').disabled = !(window.AudioContext && AudioContext.prototype.setSinkId);
  if (playCtx && $('spkDevice').value) applySpeaker();
  if ($('spkDevice').disabled) $('spkDevice').title = 'This browser cannot choose a speaker; change it in your system settings.';
}

async function testMic() {
  if (testing) return;
  const startedHere = !micOn;
  stopPlayback(); stopReplay(); send({ type: 'stop' });
  testing = { chunks: null, peak: 0 };  // set before the mic starts so nothing reaches the server
  $('micTestBtn').disabled = true;
  if (!micOn) await startMic();
  if (!micOn) { testing = null; $('micTestBtn').disabled = false; $('micTestText').textContent = 'The microphone could not be started. See the message at the top.'; return; }
  testing.chunks = [];
  for (let s = 4; s > 0; s--) { $('micTestText').textContent = `Speak now… ${s}`; await new Promise((r) => setTimeout(r, 1000)); }
  const chunks = testing.chunks, peak = testing.peak;
  testing.chunks = null; // stop collecting, but keep the server muted during playback
  const n = chunks.reduce((a, c) => a + c.length, 0);
  if (!n) { testing = null; $('micTestBtn').disabled = false; $('micTestText').textContent = 'No audio arrived from the microphone.'; if (startedHere) stopMic(); return; }
  ensurePlayCtx();
  const buf = playCtx.createBuffer(1, n, 16000), ch = buf.getChannelData(0);
  let o = 0;
  for (const c of chunks) for (let i = 0; i < c.length; i++) ch[o++] = c[i] / 32768;
  const src = playCtx.createBufferSource();
  src.buffer = buf; src.connect(playCtx.destination);
  $('micTestText').textContent = 'Playing it back…';
  src.onended = () => {
    testing = null;
    $('micTestBtn').disabled = false;
    const pct = Math.round(peak * 100);
    $('micTestText').textContent = peak < 0.02 ? `Almost silent (peak ${pct}%). Check the right microphone is chosen and not muted.`
      : peak < 0.1 ? `Quiet (peak ${pct}%). Move closer, or set Sensitivity to High.`
      : peak > 0.98 ? `Too loud (peak ${pct}%): it is distorting. Move back a little.`
      : `Sounds good (peak ${pct}%).`;
    if (startedHere) stopMic();
  };
  src.start();
}

const THEMES = [['system', 'System', 'linear-gradient(135deg,#f4f3ef 50%,#121514 50%)'], ['light', 'Light', '#f4f3ef'], ['dark', 'Dark', '#121514'],
  ['midnight', 'Midnight', '#0b1020'], ['forest', 'Forest', '#16241c'], ['sunset', 'Sunset', '#c2410c'], ['rose', 'Rose', '#be185d']];

function renderThemes() {
  const current = store.get('voiceAgentTheme') || 'system';
  $('themes').innerHTML = '';
  for (const [id, name, colour] of THEMES) {
    const b = el('button', null, 'swatch' + (id === current ? ' on' : ''));
    const dot = el('i'); dot.style.background = colour;
    b.append(dot, document.createTextNode(name));
    b.setAttribute('aria-pressed', String(id === current));
    b.onclick = () => { store.set('voiceAgentTheme', id); window.applyTheme(id); renderThemes(); };
    $('themes').append(b);
  }
}

function openInstructions() {
  $('instrGlobal').value = hello?.instructions || '';
  $('instrConv').value = conv?.instructions || '';
  $('instrConv').disabled = !conv;
  $('instrConvLabel').textContent = conv ? `For this conversation only (${conv.title})` : 'For this conversation only (open a conversation first)';
  $('instrGlobalCount').textContent = $('instrGlobal').value.length;
  $('instrConvCount').textContent = $('instrConv').value.length;
  $('instrDlg').showModal();
}

function animate() {
  const level = playing.size ? playLevel : micOn ? micLevel : 0;
  $('meterBar').style.width = (micOn ? Math.min(100, micLevel * 130) : 0).toFixed(0) + '%';
  $('meterBar').classList.toggle('hot', micLevel > 0.9);
  $('micBtn').style.setProperty('--level', Math.min(1, level * 1.5).toFixed(3));
  micLevel *= 0.85;
  requestAnimationFrame(animate);
}

// ---------- drawers ----------

const wide = () => window.matchMedia('(min-width: 1101px)').matches;

function openDetails(tab) {
  $('details').classList.remove('closed');
  if (tab) selectTab(tab);
  if (!wide()) { $('scrim').hidden = false; $('scrim').classList.remove('for-side'); }
  if (!$('tab-logs').hidden) clearErrBadge();
}
function closeDetails() { $('details').classList.add('closed'); $('scrim').hidden = true; }
function selectTab(name) {
  for (const t of document.querySelectorAll('.tab')) t.classList.toggle('on', t.dataset.tab === name);
  for (const p of document.querySelectorAll('.tabpane')) p.hidden = p.id !== 'tab-' + name;
  if (name === 'logs') { clearErrBadge(); $('log').scrollTop = $('log').scrollHeight; }
}
function clearErrBadge() { errCount = 0; $('errBadge').hidden = true; }
function openSide() { $('sidebar').classList.add('open'); $('scrim').hidden = false; $('scrim').classList.add('for-side'); }
function closeSide() { $('sidebar').classList.remove('open'); if ($('details').classList.contains('closed') || wide()) $('scrim').hidden = true; }

// ---------- controls ----------

let micBusy = false; // a second click while the microphone is still opening must not open it twice
$('micBtn').onclick = async () => {
  if (micBusy || testing) return;
  micBusy = true;
  try { if (micOn) stopMic(); else await startMic(); } finally { micBusy = false; }
};
$('textForm').onsubmit = (e) => {
  e.preventDefault();
  const text = $('textInput').value.trim();
  if (!text) return;
  ensurePlayCtx();
  if (send({ type: 'text', text })) $('textInput').value = '';
};
$('newBtn').onclick = () => { closeSide(); stopPlayback(); send({ type: 'close' }); };
$('stopBtn').onclick = () => { stopPlayback(); send({ type: 'stop' }); };
$('menuBtn').onclick = openSide;
$('detailsBtn').onclick = () => ($('details').classList.contains('closed') ? openDetails() : closeDetails());
$('costChip').onclick = () => openDetails('cost');
$('detailsClose').onclick = closeDetails;
$('scrim').onclick = () => { closeSide(); if (!wide()) closeDetails(); };
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') { closeSide(); if (!wide()) closeDetails(); } });
for (const t of document.querySelectorAll('.tab')) t.onclick = () => selectTab(t.dataset.tab);

$('preset').onchange = (e) => send({ type: 'set_preset', name: e.target.value });
$('voice').onchange = (e) => { store.set('voiceAgentVoice', e.target.value); sendSettings(); };
$('speed').onchange = (e) => { store.set('voiceAgentSpeed', e.target.value); sendSettings(); };
$('pause').onchange = (e) => { store.set('voiceAgentPause', e.target.value); sendSettings(); };
$('recordToggle').onchange = (e) => { store.set('voiceAgentRecord', e.target.checked ? null : 'off'); sendSettings(); };
$('search').oninput = renderConvList;
$('sensitivity').onchange = (e) => { store.set('voiceAgentSensitivity', e.target.value); sendSettings(); };
$('micDevice').onchange = async (e) => {
  store.set('voiceAgentMic', e.target.value);
  if (micOn) { stopMic(); await startMic(); } // restart on the new microphone
};
$('spkDevice').onchange = (e) => { store.set('voiceAgentSpk', e.target.value); applySpeaker(); };
$('micTestBtn').onclick = testMic;
navigator.mediaDevices?.addEventListener?.('devicechange', refreshDevices);
$('instrBtn').onclick = openInstructions;
$('instrBtn2').onclick = openInstructions;
$('instrGlobal').oninput = (e) => { $('instrGlobalCount').textContent = e.target.value.length; };
$('instrConv').oninput = (e) => { $('instrConvCount').textContent = e.target.value.length; };
$('instrForm').onsubmit = () => {
  const msg = { type: 'set_instructions', global: $('instrGlobal').value };
  if (conv) msg.conversation = $('instrConv').value;
  send(msg);
};
$('reviewBtn').onclick = () => {
  $('reviewDlg').showModal();
  if (conv?.review) showReview({ review: conv.review }); else send({ type: 'review' });
};
$('reviewAgain').onclick = () => send({ type: 'review' });
for (const b of document.querySelectorAll('[data-close]')) b.onclick = () => b.closest('dialog').close();
$('modeForm').onsubmit = () => {
  send({ type: 'save_mode', name: $('modeName').value, instructions: $('modeInstr').value, greeting: $('modeGreet').value });
  $('modeForm').reset();
};
$('audioBtn').onclick = async () => {
  if (!conv) { showBanner('Open a conversation first.'); return; }
  const r = await fetch(`/api/conversations/${conv.id}/recording.wav`).catch(() => null);
  if (!r || !r.ok) { showBanner(r && r.status === 404 ? 'This conversation has no recorded audio yet.' : 'Could not download the recording.'); return; }
  download(`${conv.title.replace(/[^\w -]+/g, '').trim() || 'conversation'}.wav`, await r.blob());
};
$('rawToggle').onchange = (e) => send({ type: 'raw', enabled: e.target.checked });
$('errOnly').onchange = (e) => $('log').classList.toggle('erronly', e.target.checked);
$('logClear').onclick = () => { $('log').innerHTML = ''; };

function download(name, blob) {
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

$('exportBtn').onclick = () => {
  const rows = [...document.querySelectorAll('#transcript .row:not(.error)')];
  if (!conv || !rows.length) { showBanner('There is no conversation to export yet.'); return; }
  const lines = rows.map((r) => `${r.classList.contains('user') ? 'You' : 'Assistant'}: ${r.firstChild.innerText.replace(/\s*\n\s*/g, ' ')}`);
  download(`${conv.title.replace(/[^\w -]+/g, '').trim() || 'conversation'}.txt`,
    new Blob([`${conv.title} (${modeName(conv.mode)})\n\n${lines.join('\n\n')}\n`], { type: 'text/plain' }));
};

$('dlBtn').onclick = async () => {
  if (!hello) return;
  const r = await fetch(`/api/sessions/${hello.session_id}`).catch(() => null);
  if (!r || !r.ok) {
    showBanner(r && r.status === 404 ? 'No debug log yet: it is written after the first turn of this visit.' : 'Could not download the debug log.');
    return;
  }
  download(`${hello.session_id}.json`, await r.blob());
};

if (!wide()) $('details').classList.add('closed');
// Rotating or shrinking the window must not leave the drawer covering the page.
window.matchMedia('(min-width: 1101px)').addEventListener('change', (e) => { if (!e.matches) closeDetails(); else $('scrim').hidden = true; });
renderThemes();
refreshDevices();
renderState();
connect();
animate();
setInterval(renderConvList, 60000); // keep "5 min ago" fresh
