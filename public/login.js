'use strict';
const f = document.getElementById('f'), b = document.getElementById('b'), err = document.getElementById('err');
f.addEventListener('submit', async (e) => {
  e.preventDefault(); b.disabled = true; err.textContent = '';
  try {
    const r = await fetch('/login', { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password: document.getElementById('pw').value }) });
    if (r.ok) { location.href = '/'; return; }
    err.textContent = (await r.json()).error || 'Sign in failed';
  } catch { err.textContent = 'Cannot reach the server'; }
  b.disabled = false;
});
