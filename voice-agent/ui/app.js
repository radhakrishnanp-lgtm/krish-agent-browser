// Voice Agent UI. Talks to the server over one WebSocket:
//   browser -> server: binary = 16 kHz mono Int16 PCM microphone audio; JSON = control messages
//   server -> browser: binary = TTS audio (PCM, Int16, mono); JSON = status / transcript / cost / log
'use strict';

const $ = (id) => document.getElementById(id);
const money = (v) => (v == null ? '–' : '$' + Number(v).toFixed(6));
const STATE_LABELS = {
  idle: 'Idle', listening: 'Listening', user_speaking: 'Hearing you…', transcribing: 'Transcribing…',
  thinking: 'Thinking…', speaking: 'Speaking',
};

let ws = null;
let hello = null;
let serverState = 'idle';
let micOn = false, micCtx = null, micStream = null, micLevel = 0;
let playCtx = null, playHead = 0, playing = new Set(), acceptAudio = false, outRate = 24000, playLevel = 0;
let logLines = 0;

// ---------- connection ----------

function connect() {
  setConn(false, 'Connecting…');
  const token = localStorage.getItem('voiceAgentToken') || '';
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  ws = new WebSocket(`${proto}://${location.host}/ws?token=${encodeURIComponent(token)}`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => setConn(true, 'Connected');
  ws.onmessage = (ev) => (typeof ev.data === 'string' ? onMessage(JSON.parse(ev.data)) : onAudio(ev.data));
  ws.onerror = () => addLog({ level: 'error', message: 'WebSocket error: could not reach the server.' });
  ws.onclose = (ev) => {
    stopPlayback();
    setState('idle');
    if (ev.code === 4401) {
      setConn(false, 'Access token needed');
      const t = prompt('This server needs an access token (ACCESS_TOKEN in its .env):');
      if (t) { localStorage.setItem('voiceAgentToken', t.trim()); connect(); }
      return;
    }
    setConn(false, 'Disconnected');
    addLog({ level: 'error', message: `Connection closed (code ${ev.code}). A reconnect starts a new session.` });
    showBanner(`Disconnected from the server${ev.reason ? ': ' + ev.reason : ''}.`, 'Reconnect', connect);
  };
}

function send(obj) {
  if (ws && ws.readyState === WebSocket.OPEN) { ws.send(JSON.stringify(obj)); return true; }
  addLog({ level: 'error', message: 'Not connected: message not sent.' });
  return false;
}

function setConn(on, text) {
  $('connDot').className = 'dot ' + (on ? 'on' : 'off');
  $('connText').textContent = text;
  if (on) hideBanner();
}

// ---------- server messages ----------

function onMessage(m) {
  switch (m.type) {
    case 'hello':
      hello = m; outRate = m.output_sample_rate;
      $('mLlm').textContent = m.models.llm; $('mStt').textContent = m.models.stt; $('mTts').textContent = m.models.tts;
      $('sid').textContent = m.session_id;
      $('preset').innerHTML = '';
      for (const p of m.presets) $('preset').add(new Option(p, p, false, p === m.preset));
      break;
    case 'status': setState(m.state); break;
    case 'transcript': showMessage(m); break;
    case 'cost': showCost(m); break;
    case 'log': addLog(m); break;
    case 'raw': addLog({ level: 'raw', ts: m.ts, message: `Raw ${m.stage.toUpperCase()} response`, data: m.payload }); break;
    case 'error': addBubble('error', '', m.message); break;
    case 'audio_start': acceptAudio = true; break;
    case 'interrupt': stopPlayback(); break;
  }
}

function setState(s) {
  serverState = s;
  renderState();
}

function renderState() {
  // Audio can still be playing after the server has finished the turn.
  const s = playing.size && serverState === 'listening' ? 'speaking' : serverState;
  $('orb').className = 'orb ' + s;
  let label = STATE_LABELS[s] || s;
  if (s === 'listening' && !micOn) label = 'Ready (microphone off)';
  $('state').textContent = label;
}

// ---------- transcript ----------

function addBubble(role, id, text) {
  const box = $('transcript');
  box.querySelector('.empty')?.remove();
  let el = id && document.getElementById(id);
  if (!el) {
    el = document.createElement('div');
    el.className = 'msg ' + role;
    if (id) el.id = id;
    const who = document.createElement('small');
    who.textContent = role === 'user' ? 'You' : role === 'assistant' ? 'Assistant' : 'Error';
    el.append(who, document.createElement('span'));
    box.append(el);
  }
  el.lastChild.textContent = text;
  box.scrollTop = box.scrollHeight;
  return el;
}

function showMessage(m) {
  if (!m.text) return;
  const el = addBubble(m.role, `msg-${hello?.session_id}-${m.role}-${m.turn}`, m.text);
  if (m.interrupted) el.firstChild.textContent = 'Assistant (interrupted)';
}

// ---------- cost ----------

function showCost(m) {
  const s = m.session, t = m.turn;
  $('hdrCost').textContent = money(s.total);
  $('sLlm').textContent = money(s.llm); $('sStt').textContent = money(s.stt);
  $('sTts').textContent = money(s.tts); $('sTot').textContent = money(s.total);
  if (t) {
    $('tLlm').textContent = money(t.llm_cost); $('tStt').textContent = money(t.stt_cost);
    $('tTts').textContent = money(t.tts_cost); $('tTot').textContent = money(t.turn_total);
    $('usage').textContent = `Turn ${t.index}: ${t.llm_input_tokens} in / ${t.llm_output_tokens} out tokens, ` +
      `${t.stt_seconds.toFixed(1)} s audio, ${t.tts_characters} TTS characters`;
  }
  if (m.budget_usd > 0) {
    const used = Math.min(1, s.total / m.budget_usd);
    $('budgetWrap').hidden = false;
    $('budgetBar').style.width = (used * 100).toFixed(1) + '%';
    $('budgetBar').className = used > 0.8 ? 'high' : '';
    $('budgetText').textContent = `Budget: ${money(Math.max(0, m.budget_usd - s.total))} left of $${m.budget_usd.toFixed(2)}`;
  }
}

// ---------- log panel ----------

function addLog(m) {
  const box = $('log');
  const stick = box.scrollTop + box.clientHeight >= box.scrollHeight - 20;
  const line = document.createElement('div');
  line.className = 'line ' + m.level;
  const ts = document.createElement('span'); ts.className = 'ts';
  ts.textContent = new Date(m.ts || Date.now()).toLocaleTimeString([], { hour12: false }) + ' ';
  const lvl = document.createElement('span'); lvl.className = 'lvl'; lvl.textContent = m.level.toUpperCase();
  line.append(ts, lvl, document.createTextNode(' ' + m.message));
  if (m.data !== undefined) {
    const d = document.createElement('details'), sum = document.createElement('summary'), pre = document.createElement('pre');
    sum.textContent = 'details';
    pre.textContent = JSON.stringify(m.data, null, 2);
    d.append(sum, pre); line.append(d);
  }
  box.append(line);
  while (box.childElementCount > 1000) box.firstChild.remove();
  $('logCount').textContent = `${++logLines} lines`;
  if (stick) box.scrollTop = box.scrollHeight;
  if (m.level === 'error') showBanner(m.message);
}

function showBanner(text, actionLabel, action) {
  const b = $('banner');
  b.innerHTML = '';
  const span = document.createElement('span'); span.textContent = text;
  b.append(span);
  if (action) { const a = document.createElement('button'); a.textContent = actionLabel; a.onclick = action; b.append(a); }
  const x = document.createElement('button'); x.textContent = 'Dismiss'; x.onclick = hideBanner; b.append(x);
  b.hidden = false;
}
function hideBanner() { $('banner').hidden = true; }

// ---------- playback ----------

function ensurePlayCtx() {
  if (!playCtx) playCtx = new (window.AudioContext || window.webkitAudioContext)();
  if (playCtx.state === 'suspended') playCtx.resume();
}

function onAudio(buf) {
  if (!acceptAudio) return; // stale audio from an interrupted reply
  ensurePlayCtx();
  const pcm = new Int16Array(buf);
  const audio = playCtx.createBuffer(1, pcm.length, outRate);
  const ch = audio.getChannelData(0);
  let peak = 0;
  for (let i = 0; i < pcm.length; i++) { ch[i] = pcm[i] / 32768; const a = Math.abs(ch[i]); if (a > peak) peak = a; }
  const src = playCtx.createBufferSource();
  src.buffer = audio; src.connect(playCtx.destination);
  playHead = Math.max(playHead, playCtx.currentTime + 0.05);
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
    showBanner('This browser cannot open the microphone here. Use https:// or http://localhost.');
    return;
  }
  try {
    ensurePlayCtx();
    micStream = await navigator.mediaDevices.getUserMedia({
      audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true },
    });
    micCtx = new (window.AudioContext || window.webkitAudioContext)();
    await micCtx.audioWorklet.addModule('mic-worklet.js');
    const node = new AudioWorkletNode(micCtx, 'mic-processor');
    node.port.onmessage = (ev) => {
      micLevel = ev.data.peak;
      // With barge-in off, do not send the assistant's own voice back to the server.
      if (hello && !hello.barge_in && playing.size) return;
      if (ws && ws.readyState === WebSocket.OPEN) ws.send(ev.data.pcm);
    };
    micCtx.createMediaStreamSource(micStream).connect(node);
    micOn = true;
    addLog({ level: 'info', message: `Microphone on (${micCtx.sampleRate} Hz, sent as 16000 Hz)` });
  } catch (e) {
    const why = e.name === 'NotAllowedError' ? 'permission was denied. Allow the microphone for this site and try again.'
      : e.name === 'NotFoundError' ? 'no microphone was found.' : e.message;
    addLog({ level: 'error', message: `Microphone failed: ${why}` });
    stopMic();
  }
  renderMic();
}

function stopMic() {
  micStream?.getTracks().forEach((t) => t.stop());
  micCtx?.close();
  micStream = micCtx = null; micOn = false; micLevel = 0;
  renderMic();
}

function renderMic() {
  $('micBtn').textContent = micOn ? 'Stop microphone' : 'Start microphone';
  $('micBtn').classList.toggle('on', micOn);
  renderState();
}

function animate() {
  const level = playing.size ? playLevel : micOn ? micLevel : 0;
  $('orb').style.setProperty('--level', Math.min(1, level * 1.5).toFixed(3));
  micLevel *= 0.85;
  requestAnimationFrame(animate);
}

// ---------- controls ----------

$('micBtn').onclick = () => (micOn ? stopMic() : startMic());
$('textForm').onsubmit = (e) => {
  e.preventDefault();
  const text = $('textInput').value.trim();
  if (!text) return;
  ensurePlayCtx();
  if (send({ type: 'text', text })) $('textInput').value = '';
};
$('preset').onchange = (e) => send({ type: 'set_preset', name: e.target.value });
$('stopBtn').onclick = () => { stopPlayback(); send({ type: 'stop' }); };
$('clearBtn').onclick = () => {
  stopPlayback();
  if (send({ type: 'clear' })) $('transcript').innerHTML = '<p class="empty">Conversation cleared.</p>';
};
$('rawToggle').onchange = (e) => send({ type: 'raw', enabled: e.target.checked });
$('dlBtn').onclick = async () => {
  if (!hello) return;
  const token = localStorage.getItem('voiceAgentToken') || '';
  const r = await fetch(`/api/sessions/${hello.session_id}?token=${encodeURIComponent(token)}`).catch(() => null);
  if (!r || !r.ok) {
    addLog({ level: 'warn', message: r && r.status === 404 ? 'No session log yet: it is written after the first turn.' : 'Could not download the session log.' });
    return;
  }
  const a = document.createElement('a');
  a.href = URL.createObjectURL(await r.blob());
  a.download = `${hello.session_id}.json`;
  a.click();
  URL.revokeObjectURL(a.href);
};
$('logToggle').onclick = () => {
  const collapsed = $('log').classList.toggle('collapsed');
  $('logToggle').textContent = (collapsed ? '▸' : '▾') + ' Logs';
  $('logToggle').setAttribute('aria-expanded', String(!collapsed));
};
$('errOnly').onchange = (e) => $('log').classList.toggle('erronly', e.target.checked);
$('logClear').onclick = () => { $('log').innerHTML = ''; };

connect();
animate();
