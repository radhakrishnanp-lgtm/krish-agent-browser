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

The agent gets the standard Playwright MCP tools: `browser_navigate`, `browser_snapshot` (read the page), `browser_click`, `browser_type`, `browser_tabs`, `browser_take_screenshot` and more. Tip for agents: call `browser_snapshot` after navigating to read the page, and open a new tab with `browser_tabs` so the owner's tab is left alone.

## Web UI

| Button | What it does |
|---|---|
| Pause agent / Resume agent | Blocks or allows agent actions. |
| Paste | Sends this computer's clipboard text into the remote browser. Text you copy inside the remote browser arrives in your clipboard automatically. |
| Panel | Shows or hides the side panel (more room for the browser). |
| Restart browser | Restarts Chromium if it gets stuck. Tabs are restored. |

The remote browser resizes itself to fit your window.

## Security

This app is a browser holding your logins, reachable from the internet. Treat both secrets like bank passwords.

- The web UI needs `UI_PASSWORD`. Eight wrong tries from one address lock that address out for 15 minutes.
- The MCP endpoint needs `MCP_TOKEN`. Anyone who has it can act as you on every site the browser is signed in to. Give it only to agents you trust, and change it (then redeploy) if it leaks.
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
| `MCP_EXTRA_ARGS` | no | Extra Playwright MCP options, for example `--caps vision,pdf`. |

Data (browser profile, downloads, agent screenshots) lives in the `browser-data` volume and survives redeploys.

## Run locally

```bash
cp .env.example .env
```

Edit `.env`, add `ports: ["3000:3000"]` under the `browser` service, then:

```bash
docker compose up --build
```

Open `http://localhost:3000`.
