# Security

Reviewed on 2026-11-16. Every check below was run again after the server was rewritten in Rust, with the same results. This lists what was checked, what was fixed, and what is still your
responsibility when you deploy.

## Fixed in this review

| Problem found | Fix |
|---|---|
| The access token travelled in URLs (`/ws?token=...`) and was written to the server log | You now sign in once; the token is exchanged for an `HttpOnly`, `SameSite=Strict` cookie. It is no longer in any URL, in the log, or in storage the page can read |
| Any website you visited could open the voice WebSocket from your browser (cross-site WebSocket hijacking), most serious when no token was set | The server refuses WebSocket connections whose `Origin` is not the site itself |
| No browser security headers | Content-Security-Policy (own scripts only, no inline script, no framing), `X-Frame-Options: DENY`, `nosniff`, `Referrer-Policy: no-referrer`, a microphone-only Permissions-Policy |
| A client could send unlimited-size messages or open unlimited connections | WebSocket messages over 2 MB drop the connection; at most 20 connections (`server.max_connections`) |
| Request URLs were written to the server's access log | The Rust server keeps no access log |
| Recordings could fill the disk | `recording.max_total_mb` (default 2000) stops new recordings at the limit |
| Wrong-token attempts were answered instantly | Each failed sign-in waits one second |
| The automatic API documentation pages were public | Disabled |

## Checked and found sound

- **Path traversal.** Conversation, recording and log names are matched against strict patterns
  before any file is touched. Twelve traversal attempts (`../`, encoded variants, `.env`,
  config files) all returned 404 with no content.
- **Script injection (XSS).** The UI inserts all text with `textContent`, never as HTML. A mode
  named `<img src=x onerror=...>` with `<script>` in its instructions was created and shown;
  nothing executed. The Content-Security-Policy is a second barrier.
- **Malformed input.** Invalid JSON, wrong types, oversized fields and odd-length audio were
  sent to the WebSocket; the session kept working and nothing crashed.
- **Secrets.** API keys are read from the environment only, are never sent to the browser,
  and `.env` is excluded from git and from the Docker image.
- **Dependencies.** `cargo audit` on 2026-11-16 scanned 211 crates and found no known
  vulnerabilities. It gave one warning: the YAML parser's underlying library
  (`unsafe-libyaml`, via `serde_yaml`) is no longer maintained. It only ever reads the
  config files you wrote, never anything from the network. Re-run with `cargo audit`.
- **Container.** Runs as a non-root user.

## What is still up to you

- **Set `ACCESS_TOKEN`** to a long random value. Without it, anyone who can reach the server
  can use it and spend your API credits. Generate one with:

  ```bash
  openssl rand -base64 32
  ```

- **Serve over HTTPS.** The sign-in cookie is marked `Secure` only when the request arrives
  over HTTPS (directly or via `X-Forwarded-Proto`). Over plain HTTP the token and all audio
  can be read on the network.
- **One shared identity.** There are no user accounts. Everyone with the token sees every
  conversation, recording and instruction. Do not share the token with people who should not
  hear each other's recordings.
- **Data at rest is not encrypted.** Conversations and recordings are plain files under
  `data/`. Protect the disk and its backups.
- **Recording consent.** If anyone else uses your deployment, tell them their voice is saved.
- **Spending limit.** `cost.session_budget_usd` caps one page visit, not your account. Set a
  hard limit in the OpenRouter and Isoquant dashboards as well.
- **Rotate keys that were ever pasted into a chat or committed.**
- **Behind a proxy that changes the `Host` header**, the page may fail to connect; set
  `ALLOWED_ORIGINS` to the public host name (see `.env.example`).

## Custom model endpoints

"Add your own model" makes the server send requests to whatever address is typed in, and
stores any API key entered in `data/custom_models.json` (file mode 600, never sent back to a
browser). Anyone signed in can therefore make the server call addresses on its own network.
That is the point of the feature (a GPU machine on your LAN), but on a deployment shared
with people you do not fully trust, set `custom_models.enabled: false` in
`config/default.yaml`. Only `http://` and `https://` addresses are accepted.

## Not done


- No rate limiting per user beyond the connection cap and the one-second sign-in delay.
- No audit log of sign-ins.
- No penetration test against a real deployment; the checks above ran against a local server.
