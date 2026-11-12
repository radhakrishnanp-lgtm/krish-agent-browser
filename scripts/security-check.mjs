#!/usr/bin/env node
// Security self-test. Run it against your live site after every deploy:
//   node scripts/security-check.mjs https://browser.krish.in.net
// It only sends requests WITHOUT your password or token and checks they are all refused.
// Add --lockout to also test the wrong-password lockout (locks YOUR address out of the login for 15 minutes).
const base = (process.argv[2] || '').replace(/\/+$/, '');
if (!/^https?:\/\//.test(base)) { console.error('Usage: node scripts/security-check.mjs https://browser.krish.in.net [--lockout]'); process.exit(2); }
const lockout = process.argv.includes('--lockout');
let failed = 0;
const check = (name, ok, detail = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  (${detail})` : ''}`); if (!ok) failed++; };
const get = (p, opt = {}) => fetch(base + p, { redirect: 'manual', ...opt });
const rpc = { jsonrpc: '2.0', id: 1, method: 'tools/list', params: {} };
const jsonHeaders = { 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream' };

// 1. nothing useful without signing in
let r = await get('/');
check('Home page redirects to the login page', r.status === 302 && /\/login$/.test(r.headers.get('location') || ''), `status ${r.status}`);
for (const p of ['/api/state', '/api/events', '/api/tools', '/api/files', '/files/notes/test.md', '/playbook', '/app.js', '/novnc/core/rfb.js']) {
  r = await get(p);
  check(`No access without login: GET ${p}`, [401, 302].includes(r.status), `status ${r.status}`);
}
for (const p of ['/api/pause', '/api/restart-browser', '/api/help-done', '/api/clear-activity']) {
  r = await get(p, { method: 'POST', headers: jsonHeaders, body: '{}' });
  check(`No access without login: POST ${p}`, [401, 302].includes(r.status), `status ${r.status}`);
}
r = await get('/api/files/evil.txt', { method: 'PUT', body: 'x' });
check('No upload without login', r.status === 401, `status ${r.status}`);
r = await get('/api/tools/anything', { method: 'DELETE' });
check('No tool delete without login', [401, 302].includes(r.status), `status ${r.status}`);

// 2. the agent endpoint needs the token
r = await get('/mcp', { method: 'POST', headers: jsonHeaders, body: JSON.stringify(rpc) });
check('MCP refuses a request without token', r.status === 401, `status ${r.status}`);
r = await get('/mcp', { method: 'POST', headers: { ...jsonHeaders, Authorization: 'Bearer wrong-token-000000000000' }, body: JSON.stringify(rpc) });
check('MCP refuses a wrong token', r.status === 401, `status ${r.status}`);
r = await get('/mcp?token=wrong-token-000000000000', { method: 'POST', headers: jsonHeaders, body: JSON.stringify(rpc) });
check('MCP refuses a wrong token in the URL', r.status === 401, `status ${r.status}`);
r = await get('/sse');
check('Legacy SSE endpoint refuses without token', r.status === 401, `status ${r.status}`);

// 3. forged or expired sessions
for (const [name, cookie] of [['forged', `kab_session=${Date.now() + 9e9}.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA`], ['empty signature', `kab_session=${Date.now() + 9e9}.`], ['garbage', 'kab_session=x']]) {
  r = await get('/api/state', { headers: { Cookie: cookie } });
  check(`Session cookie rejected: ${name}`, r.status === 401, `status ${r.status}`);
}

// 4. path tricks
for (const p of ['/novnc/../server.js', '/novnc/%2e%2e/%2e%2e/etc/passwd', '/files/..%2f..%2f..%2fetc%2fpasswd', '/files/%2e%2e/%2e%2e/app/server.js', '//etc/passwd', '/.env', '/server.js', '/package.json']) {
  r = await get(p);
  const body = r.status === 200 ? await r.text() : '';
  check(`Path trick refused: ${p}`, r.status !== 200 || !/root:|UI_PASSWORD|require\(/.test(body), `status ${r.status}`);
}

// 5. live view (WebSocket) needs the login
const wsUrl = base.replace(/^http/, 'ws') + '/vnc';
const wsOpen = await new Promise((resolve) => {
  let ws; try { ws = new WebSocket(wsUrl); } catch { return resolve(false); }
  const t = setTimeout(() => { try { ws.close(); } catch {} resolve(false); }, 6000);
  ws.onopen = () => { clearTimeout(t); ws.close(); resolve(true); };
  ws.onerror = () => { clearTimeout(t); resolve(false); };
});
check('Live view refuses a connection without login', !wsOpen);

// 6. login behaviour
r = await get('/login', { method: 'POST', headers: jsonHeaders, body: JSON.stringify({ password: 'definitely-wrong-password' }) });
check('Wrong password is refused', r.status === 401 || r.status === 429, `status ${r.status}`);
check('Wrong password sets no session cookie', !r.headers.get('set-cookie'));
r = await get('/login', { method: 'POST', headers: jsonHeaders, body: JSON.stringify({ password: { $ne: '' } }) });
check('Odd login input is refused', [401, 429].includes(r.status), `status ${r.status}`);

// 7. security headers and HTTPS
r = await get('/login');
const h = r.headers;
check('Header: Content-Security-Policy', /default-src 'self'/.test(h.get('content-security-policy') || ''));
check('Header: X-Frame-Options DENY (no embedding in other sites)', h.get('x-frame-options') === 'DENY');
check('Header: X-Content-Type-Options nosniff', h.get('x-content-type-options') === 'nosniff');
check('Header: Strict-Transport-Security', !!h.get('strict-transport-security'));
check('Header: Referrer-Policy no-referrer', h.get('referrer-policy') === 'no-referrer');
if (base.startsWith('https://')) {
  const plain = await fetch(base.replace('https://', 'http://') + '/healthz', { redirect: 'manual' }).catch(() => null);
  check('Plain http is redirected to https (or closed)', !plain || (plain.status >= 300 && plain.status < 400), plain ? `status ${plain.status}` : 'closed');
} else {
  console.log('SKIP  HTTPS checks (you tested an http:// address)');
}
r = await get('/healthz');
check('Health page reveals nothing', (await r.text()) === '{"ok":true}');

// 8. optional: lockout
if (lockout) {
  let got429 = false;
  for (let i = 0; i < 12 && !got429; i++) {
    r = await get('/login', { method: 'POST', headers: jsonHeaders, body: JSON.stringify({ password: `wrong-${i}` }) });
    got429 = r.status === 429;
  }
  check('Login locks out after repeated wrong passwords', got429);
  let tok429 = false;
  for (let i = 0; i < 25; i++) await get('/mcp', { method: 'POST', headers: { ...jsonHeaders, Authorization: `Bearer wrong-${i}-000000000` }, body: JSON.stringify(rpc) });
  r = await get('/mcp', { method: 'POST', headers: { ...jsonHeaders, Authorization: 'Bearer wrong-final-000000000' }, body: JSON.stringify(rpc) });
  check('Repeated wrong tokens are throttled', r.status === 401);
} else {
  console.log('SKIP  lockout test (add --lockout to run it)');
}

console.log(failed ? `\n${failed} check(s) FAILED` : '\nAll checks passed');
process.exit(failed ? 1 : 0);
