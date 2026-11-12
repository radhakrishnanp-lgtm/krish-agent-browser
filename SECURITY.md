# Security review

Tested on the built Docker image. Re-run the outside checks on your live site after every deploy:

```bash
node scripts/security-check.mjs https://browser.krish.in.net
```

It sends only requests **without** your password or token and confirms each one is refused (43 checks with `--lockout`).

## What was tested

| Area | Test | Result |
|---|---|---|
| Web UI access | Every page, API, file, script and the playbook without login | Refused (401 / redirect to login) |
| Live view | WebSocket without login; with a valid login but from another website's origin | Refused |
| Sessions | Forged, empty-signature and garbage session cookies | Refused |
| Login | Wrong password, non-text password, repeated tries | Refused; address locked after 8 tries, whole login after 40, for 15 minutes |
| Agent endpoint | No token, wrong token (header and URL), legacy `/sse` | Refused; an address is throttled after 20 wrong tokens |
| Path tricks | `../`, encoded `%2e%2e`, `%2f`, absolute paths, on `/files`, `/novnc`, uploads, `file_read`, `file_write`, custom tool names | Cannot leave the shared Files folder or the viewer folder |
| Server-side fetch (`web_fetch`, `deep_search`) | `127.0.0.1`, `localhost`, cloud metadata `169.254.169.254`, IPv6-mapped loopback, `file://` | Refused |
| Script injection | HTML/script inside agent messages, tool names, file names, activity log | Shown as plain text; a Content-Security-Policy also blocks inline scripts |
| Custom tool arguments | Quote-breaking payload in arguments | Passed as data, not executed |
| Secrets | Environment of the browser and agent-tool processes | `UI_PASSWORD` and `MCP_TOKEN` are not present |
| Dependencies | `npm audit` | 0 known vulnerabilities |
| Browser debug port, internal MCP port, VNC port | Reachable from outside the container? | No. All three listen on `127.0.0.1` inside the container only; only port 3000 is exposed, and only to Traefik |

## Fixed during this review

1. The browser and agent-tool processes inherited `UI_PASSWORD` and `MCP_TOKEN`. They no longer do.
2. Wrong MCP tokens were not rate-limited. They are now, per address.
3. Login lockout was per address only. A whole-site limit was added so many addresses cannot guess in parallel.
4. The live-view WebSocket relied only on the cookie's SameSite rule. It now also checks the Origin.
5. No Content-Security-Policy or HSTS header. Both added; the login page's inline script was moved to a file so inline scripts can be blocked.

## Risks that remain (by design) and how to live with them

| Risk | Why it exists | What to do |
|---|---|---|
| **The MCP token is as powerful as you.** Its holder drives a browser that is signed in to your accounts, and `browser_run_code_unsafe` / custom tools run code inside the container. | That is the point of an agent browser. | Give the token only to agents you trust. Rotate it (change `MCP_TOKEN`, redeploy) if it may have leaked. |
| **Prompt injection.** A web page can contain text that tries to trick an agent into misusing your logins. | Agents read untrusted pages. | The playbook tells agents to treat page text as data and to use `ask_owner` for payments, messages and passwords, but no instruction is a guarantee. Do not keep banking or your main email signed in while agents run unattended. Watch the Activity panel; use Pause. |
| **Custom tools persist.** A tricked agent could save a harmful tool that later sessions run. | Agents can create their own tools. | Every saved tool is visible, with its code, under Connect → Tools made by agents. Delete anything you do not recognise. |
| **Chromium runs with `--no-sandbox`.** A browser exploit lands in the container as the non-root `node` user. | Chromium's sandbox needs privileges a normal container does not have. | Redeploy now and then to pick up Chromium security updates (the image installs the latest Debian build). Do not mount the Docker socket or host folders into this app. |
| **Token in the URL** (`/mcp?token=…`) can end up in logs. | Some MCP clients cannot send headers. | Prefer the `Authorization` header. |
| **The agent's browser can open internal addresses** (other containers on the Dokploy network). `web_fetch` blocks them, the browser itself does not. | It is a real browser. | Keep admin panels of other apps behind their own passwords. |
| **One password, no second factor.** | Kept simple. | Use a long unique `UI_PASSWORD`. For more, put Cloudflare Access in front of the site (then the DNS record must be proxied, and `/mcp` needs a bypass rule or service token). |
| **Add-ons** can read every page. | Standard browser behaviour. | Install only add-ons you trust. |

## Reporting a problem

Open a private issue or contact the repository owner. Do not post tokens or passwords in issues.
