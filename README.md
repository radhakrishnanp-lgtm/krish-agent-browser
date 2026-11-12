# Krish Agent Browser

Your own browser, running on your VPS, that you **and** your AI agents share.

- Open `https://browser.krish.in.net` from any computer → you see a real Chromium browser and can use it like a normal browser (tabs, address bar, logins, downloads).
- Any agent or app that speaks **MCP** connects to `https://browser.krish.in.net/mcp` → it controls that same browser (open pages, click, type, read the page, take screenshots).
- While the agent works, you watch it live. A side panel lists every action the agent takes.
- A **Pause agent** button stops agents instantly whenever you want the browser for yourself.

```
 your laptop / phone                        coding agent / app (Grok, Claude, …)
        │  https (password)                         │  https + token (MCP)
        ▼                                           ▼
 ┌───────────────────────── browser.krish.in.net ─────────────────────────┐
 │  web UI + live view   ─────►   ONE Chromium   ◄─────   Playwright MCP   │
 │                                (logins saved in a Docker volume)       │
 └────────────────────────────────────────────────────────────────────────┘
```

## How "you and the agent at the same time" works

It is one browser with many tabs, like the browser on your own computer.

- The agent works in its tab. You open your own tab (`Ctrl+T`) and browse there. The agent's tab keeps running in the background at full speed.
- Click the agent's tab any time to watch it. Click your tab to go back.
- Because it is the same browser, **the agent is signed in wherever you are signed in**. Sign in to a site once by hand and the agent can use it. That is the main benefit, and also the main thing to be careful with (see Security).
- If you and the agent would use the *same* tab together, press **Pause agent** first. While paused, every agent action is refused with a clear message and nothing is clicked.

## It is a full, real browser

Everything is saved in the `browser-data` volume and survives redeploys.

| Feature | How |
|---|---|
| History | `Ctrl+H` |
| Bookmarks | `Ctrl+D` to add, `Ctrl+Shift+B` for the bar |
| Add-ons (extensions) | Open the [Chrome Web Store](https://chromewebstore.google.com) in the remote browser and press **Add to Chrome**. Ignore the "Switch to Chrome" banner. |
| Saved passwords, autofill, cookies, logins | Work as usual and are kept. |
| Downloads | Saved on the VPS. Get them on your own computer from the **Files** tab. |
| Uploads | **Files → Upload file**, then pick it from the `Downloads` folder in the remote browser's "choose file" dialog. |
| Settings, dev tools, incognito | `chrome://settings`, `F12`, `Ctrl+Shift+N` |

Your agent uses this same browser, so it gets your logins, your add-ons (for example an ad blocker) and your settings.

## Deploy on Dokploy

### 1. DNS record (Cloudflare)

| Field | Value |
|---|---|
| Type | `A` |
| Name | `browser` |
| Value | your VPS IP address |
| Proxy | **DNS only** (grey cloud) |

Grey cloud is the simple, safe choice: Dokploy's Traefik gets the Let's Encrypt certificate by itself, and the live view (a long WebSocket connection) is not cut off by Cloudflare.

### 2. Dokploy app

1. **Create Service → Compose** (type: Docker Compose).
2. **Source**: GitHub → this repository, the branch you want to deploy. Compose path: `./docker-compose.yml`.
3. **Environment** tab, add:

   ```
   UI_PASSWORD=<a long password you choose>
   MCP_TOKEN=<output of: openssl rand -hex 32>
   PUBLIC_URL=https://browser.krish.in.net
   ```

4. **Domains** tab → Add Domain:

   | Field | Value |
   |---|---|
   | Service name | `browser` |
   | Host | `browser.krish.in.net` |
   | Path | `/` |
   | Container port | `3000` |
   | HTTPS | On |
   | Certificate | Let's Encrypt |

5. Press **Deploy**. The first build takes a few minutes (it installs Chromium).

### 3. Check

```bash
curl -s https://browser.krish.in.net/healthz
```

It must print `{"ok":true}`. Then open `https://browser.krish.in.net` and sign in with `UI_PASSWORD`.

If it does not load:

| What you see | Fix |
|---|---|
| Certificate warning | DNS was not ready when Traefik asked for the certificate. Wait 2 minutes, then redeploy. |
| `404 page not found` | The domain is not attached. Check Service name `browser` and port `3000` in the Domains tab, then redeploy. |
| `Bad Gateway` | The container is still starting or crashed. Open the Dokploy logs. `Refusing to start` means `UI_PASSWORD` / `MCP_TOKEN` are missing or shorter than 12 characters. |
| Page loads, live view says "Reconnecting…" | Something in front is blocking WebSockets. Keep the Cloudflare record on grey cloud. |

## Connect an agent

In the web UI open **Connect an agent**. It shows the URL, the token and a ready config to copy.

Most MCP clients:

```json
{
  "mcpServers": {
    "krish-browser": {
      "type": "http",
      "url": "https://browser.krish.in.net/mcp",
      "headers": { "Authorization": "Bearer YOUR_MCP_TOKEN" }
    }
  }
}
```

Claude Code:

```bash
claude mcp add --transport http krish-browser https://browser.krish.in.net/mcp --header "Authorization: Bearer YOUR_MCP_TOKEN"
```

A client that only accepts a URL: `https://browser.krish.in.net/mcp?token=YOUR_MCP_TOKEN`

The agent gets 45 tools. On connect it is told to call `agent_playbook` first.

**Added by this app**

| Tool | What it does |
|---|---|
| `agent_playbook` | The rules and how-to for this browser ([AGENT_PLAYBOOK.md](AGENT_PLAYBOOK.md)). |
| `deep_search` | Research in one call: several searches, reads the top pages in parallel, returns text with sources. |
| `web_search` | Search results (titles, URLs, snippets) without touching the browser. |
| `web_fetch` | Readable text of a public page, without touching the browser. |
| `page_text` | Readable text of the current tab (after JavaScript, with your logins). Far smaller than a snapshot. |
| `files_list`, `file_read`, `file_write` | Shared Files folder: downloads, reports, and the agent's own notes (`notes/`), which give it a memory between sessions. |
| `ask_owner` | The agent asks you for help (login, CAPTCHA, 2FA, a decision). A blue bar appears above the browser; you act, optionally type a reply, press **Done**. |
| `custom_tool_save`, `custom_tool_list`, `custom_tool_delete`, `custom_tool_run` | The agent builds its own reusable tools (Playwright code). Saved tools appear as `x_<name>` in every later session. |

**Playwright browser tools (32)**

| Group | Tools |
|---|---|
| Pages and tabs | `browser_navigate`, `browser_navigate_back`, `browser_tabs`, `browser_wait_for` |
| Read the page | `browser_snapshot`, `browser_find`, `browser_take_screenshot`, `browser_console_messages`, `browser_network_requests` |
| Act | `browser_click`, `browser_type`, `browser_fill_form`, `browser_press_key`, `browser_select_option`, `browser_hover`, `browser_drag`, `browser_handle_dialog` |
| Mouse by coordinates | `browser_mouse_click_xy`, `browser_mouse_move_xy`, `browser_mouse_drag_xy`, `browser_mouse_wheel` |
| Files | `browser_file_upload`, `browser_pdf_save` |
| Scripts | `browser_evaluate`, `browser_run_code_unsafe` |

### Agents that make their own tools

When an agent needs something that does not exist, it writes a small Playwright function and saves it with `custom_tool_save`. From then on every agent that connects can call it. You see each saved tool, with its code, under **Connect → Tools made by agents**, and can delete it there.

### Search

`web_search` and `deep_search` use DuckDuckGo, then Bing, with no API key. If those block your VPS address, deploy your own [SearXNG](https://docs.searxng.org) (enable its JSON format) and set `SEARXNG_URL`.

### Files for agents

Tool results mention files like `agent-output/page-….pdf`. Besides the file tools, the agent can fetch them over HTTPS with the same token:

| Request | Result |
|---|---|
| `GET /api/files` | List of all files |
| `GET /files/agent-output/page-….pdf` | Download one file (PDFs, screenshots, browser downloads) |
| `PUT /api/files/<name>` | Upload a file. The reply gives `browserPath`, which the agent passes to `browser_file_upload`. |

## Web UI

| Button | What it does |
|---|---|
| Pause agent / Resume agent | Blocks or allows agent actions. |
| Paste | Same as `Ctrl+V`, for touch screens. |
| Files tab | Download files from the VPS to your computer, upload files, delete files. |
| Panel | Shows or hides the side panel (more room for the browser). |
| Restart browser | Restarts Chromium if it gets stuck. Tabs are restored. |

The remote browser resizes itself to fit your window.

### Copy, paste and drag and drop between your computer and the remote browser

| You do this on your computer | What happens in the remote browser |
|---|---|
| Copy text, then `Ctrl+V` (`Cmd+V` on Mac) on the remote screen | The text is pasted. All languages work. |
| Copy an image or take a screenshot, then `Ctrl+V` | The image is pasted (chat boxes, editors, image fields). |
| Copy a file in your file manager (video, PDF, anything), then `Ctrl+V` | The file is uploaded and pasted as a file. It is also kept in **Files → uploads**. |
| Drag files (or selected text) from your computer onto the remote screen | They are dropped at that exact spot in the page, for example on a site's "drop files here" area. Big files show upload progress. |
| Copy text inside the remote browser (`Ctrl+C`) | It arrives in your computer's clipboard. |

Notes: if the spot does not accept drops, the file is still saved in **Files → uploads**, so you can pick it with the site's upload button. Images and files copied *inside* the remote browser do not come back through the clipboard; download them and use the **Files** tab. Copy back to your computer needs HTTPS, which you have on your domain.

## Security

Full test results, fixes and remaining risks: [SECURITY.md](SECURITY.md). Check your live site any time with:

```bash
node scripts/security-check.mjs https://browser.krish.in.net
```

This app is a browser holding your logins, reachable from the internet. Treat both secrets like bank passwords.

- The web UI needs `UI_PASSWORD`. Eight wrong tries from one address lock that address out for 15 minutes; 40 wrong tries in total lock the login for everyone for 15 minutes.
- The MCP endpoint needs `MCP_TOKEN`. Anyone who has it can act as you on every site the browser is signed in to. Give it only to agents you trust, and change it (then redeploy) if it leaks.
- Add-ons can read every page you and the agent open. Install only ones you trust.
- An agent reads web pages, and a web page can contain text that tries to trick an agent. Do not keep sensitive accounts (banking, main email) signed in here while agents run unattended.
- Chromium runs with `--no-sandbox`, which is needed inside a normal Docker container. The container itself is the isolation layer, and it runs as a non-root user.

## Settings

| Variable | Needed | Meaning |
|---|---|---|
| `UI_PASSWORD` | yes | Web UI password (12+ characters). |
| `MCP_TOKEN` | yes | Token for agents (12+ characters). |
| `PUBLIC_URL` | no | Public address, used to show the right MCP URL. |
| `SCREEN_SIZE` | no | Starting screen size, default `1440x900`. |
| `START_URL` | no | A page to open on every start. |
| `SEARXNG_URL` | no | Address of your own SearXNG, used first by the search tools. |
| `MCP_CAPS` | no | Extra agent tool groups, default `vision,pdf`. |
| `CHROME_EXTRA_ARGS` | no | Extra Chromium command-line flags. |
| `MCP_EXTRA_ARGS` | no | Extra Playwright MCP options, for example `--ignore-https-errors`. |

Data (browser profile, history, add-ons, downloads, agent files) lives in the `browser-data` volume and survives redeploys.

## Run locally

```bash
cp .env.example .env
```

Edit `.env`, add `ports: ["3000:3000"]` under the `browser` service, then:

```bash
docker compose up --build
```

Open `http://localhost:3000`.
