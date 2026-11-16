# Voice Agent

Talk to an AI in your browser. All models run in the cloud; the server needs no GPU.

- **Three modes:** English practice (gently corrects your mistakes), Jokes and fun, and Anything.
  Add your own in `config/modes.yaml`.
- **Saved conversations:** every conversation is stored on the server. Pick one from the
  sidebar to resume it; the assistant remembers what was said.
- **Built for speed:** replies are spoken sentence by sentence while the LLM is still writing.
  See [docs/latency.md](docs/latency.md).
- **Live cost, logs and reply time** for every turn.

```
Browser UI (conversations, transcript, cost, logs, settings)
        |  WebSocket (/ws)
Server (FastAPI)
   VAD  Silero, ONNX, CPU      detects when you start and stop speaking
   STT  OpenRouter             speech -> text
   LLM  Isoquant               deepseek-v4.1-flash-1017-fast
   TTS  OpenRouter             text -> speech
```

## Run it locally

```bash
cd voice-agent
cp .env.example .env        # then add ISOQUANT_API_KEY and OPENROUTER_API_KEY
pip install -r requirements.txt
python3 scripts/smoke_test.py    # one call to each of TTS, STT and LLM; prints PASS/FAIL and cost
./scripts/start-server.sh
```

Open http://localhost:8080, pick a mode, tap the round microphone button and speak. You can
also type in the box at the bottom, which is handy for testing without a microphone.
Talking while the assistant speaks interrupts it.

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

Saved conversations live in `/app/data/conversations` and debug logs in `/app/logs/sessions`.
The compose file mounts both as volumes; on other platforms attach a persistent disk to
`/app/data`, or your conversation history is lost on every redeploy.

There are no user accounts: everyone who has the access token shares the same conversation list.

## Change models

Edit one line in `config/default.yaml`:

```yaml
active_preset: premium-voice
```

Presets live in `config/models.yaml`. See [docs/how-to-change-models.md](docs/how-to-change-models.md).

## Where things are

| Path | What it holds |
|---|---|
| `config/default.yaml` | Active preset, VAD and speed settings, session budget |
| `config/modes.yaml` | Conversation modes: prompt, greeting, speaking speed |
| `config/models.yaml` | Presets: which LLM / STT / TTS model each one uses |
| `config/providers.yaml` | Provider base URLs and which env var holds each key |
| `config/pricing.yaml` | Cost rates |
| `server/` | `main.py` (web server), `session.py` (turn pipeline + cost + log), `store.py` (saved conversations), `providers.py` (the three API calls), `vad.py`, `pricing.py`, `config.py` |
| `ui/` | The browser UI (plain HTML, CSS and JS, no build step) |
| `scripts/` | `start-server.sh`, `smoke_test.py` |
| `data/conversations/` | Saved conversations, one JSON file each |
| `logs/sessions/` | Debug log, one JSON file per page visit |
| `docs/` | Model changes, modes, latency, cost tracking, checkpoints |

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
| It cuts me off before I finish | Raise `vad.min_silence_ms` in `default.yaml` (try 800) |
| Replies feel slow | Open Details, Logs: each line shows how long STT, LLM and TTS took. See `docs/latency.md` |
| "Session budget reached" | Raise `cost.session_budget_usd` in `default.yaml`, or reload for a new session |
