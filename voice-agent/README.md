# Voice Agent

Talk to an AI in your browser. All models run behind HTTP APIs, in the cloud or on your own
GPU machine; the app itself needs no GPU.

**Start with [PLAYBOOK.md](PLAYBOOK.md):** how to run, use, change models, add your own GPU,
deploy and operate, plus instructions for AI coding agents.

- **Three modes:** English practice (gently corrects your mistakes), Jokes and fun, and Anything.
  Add your own in `config/modes.yaml`.
- **Saved conversations:** every conversation is stored on the server. Pick one from the
  sidebar to resume it; the assistant remembers what was said.
- **Recordings:** both sides of the conversation are recorded. Press Play under any message to
  hear it again, or download the whole conversation as one audio file.
- **English feedback report:** in English practice, the Feedback button lists your mistakes
  with corrections, an estimated level and what to practise next.
- **Your own modes:** describe how the assistant should behave ("job interviewer", "storyteller")
  and start talking.
- **Custom instructions:** tell the assistant about you and how to answer, for every
  conversation or just the open one.
- **Microphone tools:** choose the microphone and speaker, set sensitivity, and run a 4-second
  test that plays your voice back.
- **Themes:** System, Light, Dark, Midnight, Forest, Sunset and Rose.
- **21 models to choose from:** 9 language models, 6 speech-to-text and 6 text-to-speech,
  each with its price shown. Switch any of them at any time in Settings.
- **Your own models:** add any OpenAI-compatible endpoint (for example a model on your own
  GPU) from the UI; it is tested as soon as you add it.
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
   set, the UI asks for the token once and keeps you signed in with a cookie for 90 days.
   See [docs/security.md](docs/security.md) before going live.
3. **Set the keys as environment variables** on the platform (`ISOQUANT_API_KEY`,
   `OPENROUTER_API_KEY`). Never bake `.env` into the image; `.dockerignore` already excludes it.

`GET /healthz` returns the active preset and any missing keys, for health checks.

Everything you create lives under `/app/data`. The compose file keeps it in a named volume;
on other platforms attach a persistent disk to `/app/data`, or your history is lost on every
redeploy.

Recordings are WAV files in `/app/data/audio`, about 2 to 3 MB per minute of speech, and are
deleted with their conversation. Turn recording off for everyone with `recording.enabled: false`
in `config/default.yaml`, or per browser in Settings. Recordings are other people's voices if
you share the app: tell them it records.

There are no user accounts: everyone who has the access token shares the same conversation list.

## Change models

Edit one line in `config/default.yaml`:

```yaml
active_preset: fastest
```

Or change any model from the UI at any time. The catalog and prices are in
[PLAYBOOK.md](PLAYBOOK.md), part 4.

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
| `scripts/` | `start-server.sh`, `check.sh` (free checks), `smoke_test.py` (one paid call per stage) |
| `data/custom_models.json` | Models added in the UI |
| `data/conversations/` | Saved conversations, one JSON file each |
| `data/audio/` | Recordings, one folder per conversation |
| `data/custom_modes.json` | Modes created in the UI |
| `data/profile.json` | Your instructions for every conversation |
| `logs/sessions/` | Debug log, one JSON file per page visit |
| `docs/` | Model changes, modes, latency, cost tracking, security, checkpoints |

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
| It does not hear me, or reacts to background noise | Details, Settings: run "Test microphone", pick the right microphone, change Sensitivity |
| "Connected" never appears behind a proxy | Set `ALLOWED_ORIGINS` to your public host name |
| The assistant interrupts itself | Speaker sound reaches the microphone; use headphones or set `vad.barge_in: false` |
| It cuts me off before I finish | Raise `vad.min_silence_ms` in `default.yaml` (try 800) |
| Replies feel slow | Open Details, Logs: each line shows how long STT, LLM and TTS took. See `docs/latency.md` |
| "Session budget reached" | Raise `cost.session_budget_usd` in `default.yaml`, or reload for a new session |
