# Voice Agent

A live voice conversation in the browser. All models run in the cloud; the server needs no GPU.

```
Browser UI (transcript, status bar, cost panel, log panel)
        |  WebSocket (/ws)
Server (FastAPI)
   VAD  Silero, ONNX, CPU      detects when you start and stop speaking
   STT  OpenRouter             speech -> text
   LLM  Isoquant               deepseek-v4.1-flash-1017-fast
   TTS  OpenRouter             text -> speech, streamed back sentence by sentence
```

Every call's cost is tracked, shown live in the UI, printed in the terminal and saved to
`logs/sessions/<session>.json`.

## Run it locally

```bash
cd voice-agent
cp .env.example .env        # then add ISOQUANT_API_KEY and OPENROUTER_API_KEY
pip install -r requirements.txt
python3 scripts/smoke_test.py    # one call to each of TTS, STT and LLM; prints PASS/FAIL and cost
./scripts/start-server.sh
```

Open http://localhost:8080, press **Start microphone** and speak. You can also type in the box
under the transcript, which is handy for testing without a microphone.

The UI is served by the same server, so there is no separate UI start script.

## Deploy

The app is one container listening on one port (`PORT`, default 8080).

```bash
cd voice-agent
docker compose up -d --build
```

Three things matter when it is on the internet:

1. **HTTPS is required.** Browsers only allow the microphone on `https://` or `localhost`.
   Put the container behind something that terminates TLS (Caddy, nginx, Cloudflare, or a
   platform such as Render, Railway or Fly.io that does it for you). The proxy must allow
   WebSockets on `/ws`.
2. **Set `ACCESS_TOKEN`.** Without it, anyone with the URL can spend your API credits. With it
   set, the UI asks for the token once and remembers it in that browser.
3. **Set the keys as environment variables** on the platform (`ISOQUANT_API_KEY`,
   `OPENROUTER_API_KEY`). Never bake `.env` into the image; `.dockerignore` already excludes it.

`GET /healthz` returns the active preset and any missing keys, for health checks.

Session logs are written inside the container at `/app/logs/sessions`. Mount a volume there
(the compose file does) if you want to keep them.

## Change models

Edit one line in `config/default.yaml`:

```yaml
active_preset: low-cost
```

Presets live in `config/models.yaml`. See [docs/how-to-change-models.md](docs/how-to-change-models.md).

## Where things are

| Path | What it holds |
|---|---|
| `config/default.yaml` | Active preset, system prompt, VAD settings, session budget |
| `config/models.yaml` | Presets: which LLM / STT / TTS model each one uses |
| `config/providers.yaml` | Provider base URLs and which env var holds each key |
| `config/pricing.yaml` | Cost rates |
| `server/` | `main.py` (web server), `session.py` (turn pipeline + cost + log), `providers.py` (the three API calls), `vad.py`, `pricing.py`, `config.py` |
| `ui/` | The browser UI (plain HTML, CSS and JS, no build step) |
| `scripts/` | `start-server.sh`, `smoke_test.py` |
| `logs/sessions/` | One JSON file per conversation |
| `docs/` | Model changes, cost tracking, checkpoints |

## Tests

```bash
python3 -m unittest discover tests
```

## Troubleshooting

| What you see | Likely cause |
|---|---|
| Red banner "`..._API_KEY is not set`" | Key missing from `.env`; add it and restart |
| "HTTP 401 ... check the API key" | Key is wrong or revoked |
| "HTTP 402 ... out of credits" | Top up the provider account |
| "HTTP 404 ... check the model name" | Model id in `models.yaml` does not exist at that provider |
| Microphone button does nothing or errors | Page is not on HTTPS or localhost, or permission was denied |
| The assistant interrupts itself | Speaker sound reaches the microphone; use headphones or set `vad.barge_in: false` |
| "Session budget reached" | Raise `cost.session_budget_usd` in `default.yaml`, or reload for a new session |
