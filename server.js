'use strict';
/*
 * Krish Agent Browser
 *
 * One container, one real Chromium:
 *   - Xvnc      : virtual screen the browser draws on
 *   - Chromium  : the browser (persistent profile in DATA_DIR)
 *   - Playwright MCP : lets AI agents drive that same Chromium (over CDP)
 *   - this server    : password-protected web UI (live view via noVNC),
 *                      token-protected MCP endpoint, activity log, pause switch
 */
const http = require('http');
const net = require('net');
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { spawn } = require('child_process');
const { WebSocketServer } = require('ws');

// ---------------------------------------------------------------- config
const PORT = Number(process.env.PORT || 3000);
const UI_PASSWORD = process.env.UI_PASSWORD || '';
const MCP_TOKEN = process.env.MCP_TOKEN || '';
const PUBLIC_URL = (process.env.PUBLIC_URL || '').replace(/\/+$/, '');
const DATA_DIR = process.env.DATA_DIR || path.join(__dirname, 'data');
const CHROME_BIN = process.env.CHROME_BIN || '/usr/bin/chromium';
const NOVNC_DIR = process.env.NOVNC_DIR || '/usr/share/novnc';
const SCREEN_SIZE = /^\d+x\d+$/.test(process.env.SCREEN_SIZE || '') ? process.env.SCREEN_SIZE : '1440x900';
const START_URL = process.env.START_URL || ''; // optional page to open on every start
const DISPLAY = ':1';
const VNC_PORT = 5900;
const CDP_PORT = 9222;
const MCP_PORT = 8931;
const MANAGE_BROWSER = process.env.MANAGE_BROWSER !== '0'; // set 0 for local dev without a screen

function weak(v) { return v.length < 12 || /^change-me/.test(v); }
if (weak(UI_PASSWORD) || weak(MCP_TOKEN)) {
  console.error('Refusing to start: UI_PASSWORD and MCP_TOKEN must both be set, at least 12 characters, and not the example values.');
  process.exit(1);
}
const SESSION_SECRET = crypto.createHash('sha256').update('kab-session:' + UI_PASSWORD).digest();
const SESSION_DAYS = 30;

// ---------------------------------------------------------------- state
const state = { paused: false, browserUp: false, mcpUp: false, screenUp: false };
const activity = [];            // last 300 agent events
const sseClients = new Set();
let viewers = 0;

function log(...a) { console.log(new Date().toISOString(), ...a); }

function publish(type, data) {
  const msg = `event: ${type}\ndata: ${JSON.stringify(data)}\n\n`;
  for (const res of sseClients) res.write(msg);
}
function publishState() { publish('state', { ...state, viewers }); }
function addActivity(entry) {
  entry.id = crypto.randomBytes(6).toString('hex');
  entry.time = Date.now();
  activity.push(entry);
  if (activity.length > 300) activity.shift();
  publish('activity', entry);
}

// ---------------------------------------------------------------- child processes
const children = {};
let shuttingDown = false;

function run(name, cmd, args, opts = {}) {
  const start = () => {
    if (shuttingDown) return;
    log(`[${name}] starting`);
    const child = spawn(cmd, args, {
      env: { ...process.env, DISPLAY, HOME: process.env.HOME || '/home/node', ...(opts.env || {}) },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    children[name] = child;
    const pipe = (s) => s.on('data', (d) => {
      if (opts.quiet) return;
      for (const line of d.toString().split('\n')) if (line.trim()) console.log(`[${name}] ${line}`);
    });
    pipe(child.stdout); pipe(child.stderr);
    child.on('error', (e) => log(`[${name}] failed to start: ${e.message}`));
    child.on('exit', (code, sig) => {
      log(`[${name}] exited (${sig || code})`);
      if (children[name] === child) delete children[name];
      if (opts.onExit) opts.onExit();
      if (!shuttingDown) setTimeout(start, opts.restartDelay || 1500);
    });
    if (opts.onStart) opts.onStart(child);
  };
  start();
}

function tcpUp(port) {
  return new Promise((resolve) => {
    const s = net.connect({ host: '127.0.0.1', port });
    s.setTimeout(1000);
    s.once('connect', () => { s.destroy(); resolve(true); });
    s.once('error', () => resolve(false));
    s.once('timeout', () => { s.destroy(); resolve(false); });
  });
}
async function waitTcp(port, ms = 30000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await tcpUp(port)) return true;
    await new Promise((r) => setTimeout(r, 250));
  }
  return false;
}

function chromeArgs() {
  const [w, h] = SCREEN_SIZE.split('x');
  return [
    `--user-data-dir=${path.join(DATA_DIR, 'profile')}`,
    `--remote-debugging-port=${CDP_PORT}`,
    '--remote-debugging-address=127.0.0.1',
    '--no-first-run',
    '--no-default-browser-check',
    '--no-sandbox',                 // required inside an unprivileged Docker container
    '--test-type',                  // hides the "--no-sandbox" warning bar
    '--password-store=basic',
    '--start-maximized',
    '--window-position=0,0',
    `--window-size=${w},${h}`,
    '--restore-last-session',
    '--hide-crash-restore-bubble',
    // keep the agent's tab working at full speed while you look at another tab
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--disable-renderer-backgrounding',
    '--disable-features=Translate,CalculateNativeWinOcclusion',
    ...(START_URL ? [START_URL] : []), // otherwise Chromium restores the previous tabs
  ];
}

function clearProfileLocks() {
  // A redeploy changes the container hostname; stale locks would block Chromium.
  const dir = path.join(DATA_DIR, 'profile');
  for (const f of ['SingletonLock', 'SingletonCookie', 'SingletonSocket']) {
    try { fs.rmSync(path.join(dir, f), { force: true }); } catch {}
  }
}

async function startStack() {
  fs.mkdirSync(path.join(DATA_DIR, 'profile'), { recursive: true });
  fs.mkdirSync(path.join(DATA_DIR, 'agent-output'), { recursive: true });
  try { fs.rmSync('/tmp/.X1-lock', { force: true }); fs.rmSync('/tmp/.X11-unix/X1', { force: true }); } catch {}

  run('screen', 'Xvnc', [
    DISPLAY, '-geometry', SCREEN_SIZE, '-depth', '24',
    '-rfbport', String(VNC_PORT), '-localhost',
    '-SecurityTypes', 'None',       // safe: only reachable from inside the container, the web UI adds the login
    '-AlwaysShared', '-AcceptSetDesktopSize',
  ], { quiet: true, onExit: () => { state.screenUp = false; publishState(); } });
  state.screenUp = await waitTcp(VNC_PORT);
  if (!state.screenUp) log('WARNING: virtual screen did not come up');

  run('wm', 'openbox', [], { quiet: true });

  run('browser', CHROME_BIN, chromeArgs(), {
    quiet: true,
    onStart: async () => {
      clearProfileLocks();
      state.browserUp = await waitTcp(CDP_PORT, 60000);
      publishState();
      log(`[browser] ${state.browserUp ? 'ready' : 'NOT ready'}`);
    },
    onExit: () => {
      state.browserUp = false; publishState();
      clearProfileLocks();
      // MCP sessions hold a connection to the old browser: restart it so agents reconnect cleanly.
      if (children.mcp) children.mcp.kill();
    },
  });
  await waitTcp(CDP_PORT, 60000);
  startMcp();
}

function startMcp() {
  const cli = path.join(__dirname, 'node_modules', '@playwright', 'mcp', 'cli.js');
  const extra = (process.env.MCP_EXTRA_ARGS || '').split(/\s+/).filter(Boolean);
  run('mcp', process.execPath, [
    cli,
    '--cdp-endpoint', process.env.CDP_ENDPOINT || `http://127.0.0.1:${CDP_PORT}`,
    '--host', '127.0.0.1', '--port', String(MCP_PORT),
    '--output-dir', path.join(DATA_DIR, 'agent-output'),
    '--ignore-roots',
    '--snapshot-mode', 'none',   // page snapshots are returned by the browser_snapshot tool, not written to files agents cannot read
    ...extra,
  ], {
    env: { PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: '1' },
    onStart: async () => { state.mcpUp = await waitTcp(MCP_PORT); publishState(); },
    onExit: () => { state.mcpUp = false; publishState(); },
  });
}

// ---------------------------------------------------------------- auth helpers
function safeEqual(a, b) {
  const ha = crypto.createHash('sha256').update(String(a)).digest();
  const hb = crypto.createHash('sha256').update(String(b)).digest();
  return crypto.timingSafeEqual(ha, hb);
}
function sign(v) { return crypto.createHmac('sha256', SESSION_SECRET).update(v).digest('base64url'); }
function makeSession() {
  const exp = String(Date.now() + SESSION_DAYS * 86400e3);
  return `${exp}.${sign(exp)}`;
}
function cookies(req) {
  const out = {};
  for (const part of (req.headers.cookie || '').split(';')) {
    const i = part.indexOf('=');
    if (i > 0) out[part.slice(0, i).trim()] = part.slice(i + 1).trim();
  }
  return out;
}
function loggedIn(req) {
  const v = cookies(req).kab_session;
  if (!v) return false;
  const [exp, sig] = v.split('.');
  if (!exp || !sig || !safeEqual(sig, sign(exp))) return false;
  return Number(exp) > Date.now();
}
function clientIp(req) {
  return (req.headers['x-forwarded-for'] || '').split(',')[0].trim() || req.socket.remoteAddress || '?';
}
function isHttps(req) { return (req.headers['x-forwarded-proto'] || '').split(',')[0].trim() === 'https'; }

const failures = new Map(); // ip -> { count, until }
function blocked(ip) {
  const f = failures.get(ip);
  return f && f.count >= 8 && f.until > Date.now();
}
function noteFailure(ip) {
  const f = failures.get(ip);
  if (!f || f.until < Date.now()) failures.set(ip, { count: 1, until: Date.now() + 15 * 60e3 });
  else f.count++;
}
setInterval(() => { for (const [k, f] of failures) if (f.until < Date.now()) failures.delete(k); }, 60e3).unref();

function mcpAuthorized(req, url) {
  const h = req.headers.authorization || '';
  const m = /^Bearer\s+(.+)$/i.exec(h);
  if (m && safeEqual(m[1].trim(), MCP_TOKEN)) return true;
  const q = url.searchParams.get('token');
  return !!q && safeEqual(q, MCP_TOKEN);
}

// ---------------------------------------------------------------- http helpers
const SEC_HEADERS = {
  'X-Content-Type-Options': 'nosniff',
  'X-Frame-Options': 'DENY',
  'Referrer-Policy': 'no-referrer',
};
function send(res, status, body, headers = {}) {
  res.writeHead(status, { ...SEC_HEADERS, ...headers });
  res.end(body);
}
function json(res, status, obj) {
  send(res, status, JSON.stringify(obj), { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
}
function readBody(req, limit = 8 * 1024 * 1024) {
  return new Promise((resolve, reject) => {
    const chunks = []; let size = 0;
    req.on('data', (c) => {
      size += c.length;
      if (size > limit) { reject(new Error('too large')); req.destroy(); return; }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}
const MIME = {
  '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml', '.png': 'image/png', '.ico': 'image/x-icon', '.json': 'application/json',
};
function serveFile(res, root, rel, cache = 'no-cache') {
  const file = path.normalize(path.join(root, rel));
  if (file !== root && !file.startsWith(root + path.sep)) return send(res, 403, 'Forbidden');
  fs.stat(file, (err, st) => {
    if (err || !st.isFile()) return send(res, 404, 'Not found');
    res.writeHead(200, { ...SEC_HEADERS, 'Content-Type': MIME[path.extname(file)] || 'application/octet-stream', 'Cache-Control': cache });
    fs.createReadStream(file).pipe(res);
  });
}
const PUBLIC_DIR = path.join(__dirname, 'public');

// ---------------------------------------------------------------- MCP proxy
function short(v, n = 160) {
  const s = typeof v === 'string' ? v : JSON.stringify(v);
  return s && s.length > n ? s.slice(0, n) + '…' : s;
}
function describeCall(params = {}) {
  const a = params.arguments || {};
  const bits = [];
  for (const k of ['url', 'element', 'text', 'key', 'filename', 'action', 'index', 'time']) {
    if (a[k] !== undefined && a[k] !== '') bits.push(`${k}: ${short(a[k], 120)}`);
  }
  if (!bits.length && Object.keys(a).length) bits.push(short(a, 160));
  return bits.join(' · ');
}
const sessionNames = new Map(); // mcp session id -> client name

async function proxyMcp(req, res, url) {
  if (!mcpAuthorized(req, url)) {
    return send(res, 401, JSON.stringify({ error: 'Missing or wrong MCP token. Send header "Authorization: Bearer <MCP_TOKEN>".' }),
      { 'Content-Type': 'application/json', 'WWW-Authenticate': 'Bearer' });
  }
  let body = Buffer.alloc(0);
  if (req.method === 'POST') {
    try { body = await readBody(req); } catch { return send(res, 413, 'Body too large'); }
  }

  // look inside the JSON-RPC message(s) for the activity log and the pause switch
  let messages = [];
  if (body.length) {
    try { const p = JSON.parse(body.toString('utf8')); messages = Array.isArray(p) ? p : [p]; } catch {}
  }
  const sid = req.headers['mcp-session-id'];
  const client = (sid && sessionNames.get(sid)) || 'agent';
  const calls = messages.filter((m) => m && m.method === 'tools/call');

  if (calls.length && state.paused) {
    for (const m of calls) addActivity({ kind: 'blocked', client, tool: m.params && m.params.name, detail: 'blocked – agent is paused' });
    const reply = (m) => ({
      jsonrpc: '2.0', id: m.id,
      result: { isError: true, content: [{ type: 'text', text: 'The browser owner has paused agent control. No action was performed. Wait and try again later, or ask the owner to resume the agent in the web UI.' }] },
    });
    const withId = messages.filter((m) => m && m.id !== undefined);
    const out = withId.map((m) => (m.method === 'tools/call' ? reply(m) : { jsonrpc: '2.0', id: m.id, error: { code: -32000, message: 'Agent control is paused' } }));
    return json(res, 200, Array.isArray(JSON.parse(body.toString('utf8'))) ? out : out[0]);
  }
  for (const m of calls) addActivity({ kind: 'call', client, tool: m.params && m.params.name, detail: describeCall(m.params) });
  const init = messages.find((m) => m && m.method === 'initialize');

  url.searchParams.delete('token');
  const headers = { ...req.headers, host: `127.0.0.1:${MCP_PORT}` };
  delete headers.authorization;
  delete headers['content-length'];
  delete headers.origin; // the upstream only ever sees this trusted proxy
  for (const h of Object.keys(headers)) if (h.startsWith('x-forwarded-') || h === 'forwarded' || h === 'x-real-ip') delete headers[h];
  if (body.length) headers['content-length'] = String(body.length);

  const up = http.request({ host: '127.0.0.1', port: MCP_PORT, method: req.method, path: url.pathname + url.search, headers }, (r) => {
    const newSid = r.headers['mcp-session-id'];
    if (init && newSid) {
      const ci = (init.params && init.params.clientInfo) || {};
      const name = short(ci.name || 'agent', 40);
      sessionNames.set(newSid, name);
      if (sessionNames.size > 200) sessionNames.delete(sessionNames.keys().next().value);
      addActivity({ kind: 'connect', client: name, detail: `connected${ci.version ? ' (v' + short(ci.version, 20) + ')' : ''}` });
    }
    res.writeHead(r.statusCode, r.headers);
    r.pipe(res);
  });
  up.on('error', () => {
    if (!res.headersSent) json(res, 502, { error: 'Browser MCP service is starting or unavailable. Try again in a few seconds.' });
    else res.destroy();
  });
  res.on('close', () => up.destroy());
  up.end(body);
}

// ---------------------------------------------------------------- routes
const server = http.createServer(async (req, res) => {
  let url;
  try { url = new URL(req.url, 'http://localhost'); } catch { return send(res, 400, 'Bad request'); }
  const p = url.pathname;

  try {
    if (p === '/healthz') return json(res, 200, { ok: true });

    if (p === '/mcp' || p === '/sse') return await proxyMcp(req, res, url);

    if (p === '/login' && req.method === 'GET') {
      if (loggedIn(req)) return send(res, 302, '', { Location: '/' });
      return serveFile(res, PUBLIC_DIR, 'login.html');
    }
    if (p === '/login' && req.method === 'POST') {
      const ip = clientIp(req);
      if (blocked(ip)) return json(res, 429, { error: 'Too many wrong passwords. Try again in 15 minutes.' });
      let pw = '';
      try { pw = JSON.parse((await readBody(req, 4096)).toString('utf8')).password || ''; } catch {}
      if (!safeEqual(pw, UI_PASSWORD)) {
        noteFailure(ip);
        log(`login failed from ${ip}`);
        await new Promise((r) => setTimeout(r, 600));
        return json(res, 401, { error: 'Wrong password' });
      }
      failures.delete(ip);
      const cookie = `kab_session=${makeSession()}; Path=/; HttpOnly; SameSite=Strict; Max-Age=${SESSION_DAYS * 86400}${isHttps(req) ? '; Secure' : ''}`;
      return send(res, 200, JSON.stringify({ ok: true }), { 'Content-Type': 'application/json', 'Set-Cookie': cookie });
    }
    if (p === '/favicon.svg') return serveFile(res, PUBLIC_DIR, 'favicon.svg', 'max-age=86400');

    // ---- everything below needs the UI login
    if (!loggedIn(req)) {
      if (p.startsWith('/api/')) return json(res, 401, { error: 'Not logged in' });
      return send(res, 302, '', { Location: '/login' });
    }

    if (p === '/' && req.method === 'GET') return serveFile(res, PUBLIC_DIR, 'index.html');
    if (p === '/app.js' || p === '/app.css') return serveFile(res, PUBLIC_DIR, p.slice(1));
    if (p.startsWith('/novnc/') && req.method === 'GET') return serveFile(res, NOVNC_DIR, decodeURIComponent(p.slice(7)), 'max-age=3600');

    if (p === '/logout' && req.method === 'POST') {
      return send(res, 200, '{}', { 'Content-Type': 'application/json', 'Set-Cookie': 'kab_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0' });
    }

    if (p === '/api/state' && req.method === 'GET') {
      const base = PUBLIC_URL || `${isHttps(req) ? 'https' : 'http'}://${req.headers.host}`;
      return json(res, 200, { ...state, viewers, activity, mcpUrl: `${base}/mcp`, mcpToken: MCP_TOKEN });
    }
    if (p === '/api/events' && req.method === 'GET') {
      res.writeHead(200, { ...SEC_HEADERS, 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store', 'X-Accel-Buffering': 'no' });
      res.write('retry: 2000\n\n');
      sseClients.add(res);
      const ping = setInterval(() => res.write(': ping\n\n'), 25000);
      req.on('close', () => { clearInterval(ping); sseClients.delete(res); });
      return;
    }
    if (p.startsWith('/api/') && req.method === 'POST') {
      if (!/^application\/json/.test(req.headers['content-type'] || '')) return json(res, 415, { error: 'JSON only' });
      let data = {};
      try { data = JSON.parse((await readBody(req, 4096)).toString('utf8') || '{}'); } catch {}
      if (p === '/api/pause') {
        state.paused = !!data.paused;
        addActivity({ kind: 'owner', client: 'you', detail: state.paused ? 'paused the agent' : 'resumed the agent' });
        publishState();
        return json(res, 200, { paused: state.paused });
      }
      if (p === '/api/restart-browser') {
        addActivity({ kind: 'owner', client: 'you', detail: 'restarted the browser' });
        if (children.browser) children.browser.kill();
        return json(res, 200, { ok: true });
      }
      if (p === '/api/clear-activity') {
        activity.length = 0;
        publish('cleared', {});
        return json(res, 200, { ok: true });
      }
    }
    return send(res, 404, 'Not found');
  } catch (e) {
    log('request error', e.message);
    if (!res.headersSent) send(res, 500, 'Server error'); else res.destroy();
  }
});

// ---------------------------------------------------------------- live view (WebSocket <-> VNC)
const wss = new WebSocketServer({ noServer: true, perMessageDeflate: false });
server.on('upgrade', (req, socket, head) => {
  let p = '';
  try { p = new URL(req.url, 'http://localhost').pathname; } catch {}
  if (p !== '/vnc' || !loggedIn(req)) {
    socket.write('HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n');
    return socket.destroy();
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    const vnc = net.connect({ host: '127.0.0.1', port: VNC_PORT });
    vnc.setNoDelay(true);
    viewers++; publishState();
    let closed = false;
    const close = () => {
      if (closed) return; closed = true;
      viewers--; publishState();
      vnc.destroy();
      try { ws.close(); } catch {}
    };
    vnc.on('data', (d) => { if (ws.readyState === 1) ws.send(d); });
    ws.on('message', (d) => vnc.write(d));
    vnc.on('close', close); vnc.on('error', close);
    ws.on('close', close); ws.on('error', close);
  });
});

// ---------------------------------------------------------------- boot
server.listen(PORT, '0.0.0.0', () => log(`web UI + MCP listening on :${PORT}`));
if (MANAGE_BROWSER) startStack().catch((e) => { log('startup failed', e); process.exit(1); });
else startMcp();

function shutdown() {
  if (shuttingDown) return;
  shuttingDown = true;
  log('shutting down');
  for (const res of sseClients) res.end();
  const b = children.browser;
  if (b) b.kill('SIGTERM'); // lets Chromium save cookies/session cleanly
  setTimeout(() => {
    for (const c of Object.values(children)) c.kill('SIGTERM');
    server.close();
    setTimeout(() => process.exit(0), 500);
  }, b ? 2500 : 0);
}
process.on('SIGTERM', shutdown);
process.on('SIGINT', shutdown);
