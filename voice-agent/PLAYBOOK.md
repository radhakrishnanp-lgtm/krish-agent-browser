# Voice Agent Playbook

How to run, use, change and deploy the voice agent. Written so that a person or an AI coding
agent can follow it without other context. Commands are run from the `voice-agent/` folder
unless stated otherwise.

| Part | What it covers |
|---|---|
| [1. What this is](#1-what-this-is) | The system in one page |
| [2. Run it](#2-run-it) | From nothing to a working conversation |
| [3. Use it](#3-use-it) | Every feature in the UI |
| [4. Models and prices](#4-models-and-prices) | The catalog, switching models, adding one |
| [5. Your own GPU](#5-your-own-gpu) | Adding your own model endpoint |
| [6. Deploy](#6-deploy) | Docker, HTTPS, environment variables, backups |
| [7. Operate](#7-operate) | Health, logs, updating, common failures |
| [8. For AI agents](#8-for-ai-agents) | Rules, file map, task recipes, how to verify |
| [9. What is not verified](#9-what-is-not-verified) | Known gaps, stated plainly |

---

## 1. What this is

A browser app for talking with an AI. Speech goes in, speech comes out. Every model runs
behind an HTTP API, in the cloud or on your own machine; the app itself needs no GPU.

The server is one Rust program (about 9 MB, starts in under 0.1 s, about 50 MB of memory).
The UI is plain HTML, CSS and JavaScript with no build step.

```
Browser (ui/)                       one WebSocket: /ws
   microphone audio  ───────────►  Server (src/, Rust: axum + tokio)
   reply audio       ◄───────────     1. VAD    Silero, on CPU: when did you start and stop talking
   text, cost, logs  ◄───────────     2. STT    speech -> text      (HTTP API)
                                      3. LLM    text -> reply       (HTTP API, streamed)
                                      4. TTS    reply -> speech     (HTTP API, streamed)
```

All three model calls use the OpenAI API shape (`/chat/completions`,
`/audio/transcriptions`, `/audio/speech`), so any provider or local server that speaks it
can be plugged in.

What is stored, and where:

| Data | Location | Notes |
|---|---|---|
| Conversations | `data/conversations/<id>.json` | Text, cost totals, feedback report, per-conversation instructions, folder |
| Folders | `data/folders.json` | Names and order |
| Recordings | `data/audio/<id>/NNNN-user.wav`, `NNNN-assistant.wav` | 2 to 3 MB per minute of speech |
| Modes made in the UI | `data/custom_modes.json` | |
| Models added in the UI | `data/custom_models.json` | Can contain API keys; file mode 600 |
| Instructions for every conversation | `data/profile.json` | |
| Debug log per page visit | `logs/sessions/<session>.json` | |
| API keys | `.env` or the platform's environment | Never in git, never sent to the browser |

There are no user accounts. One access token protects the whole app, and everyone who has
it shares the same conversations.

---

## 2. Run it

Needs Rust (install from https://rustup.rs) or Docker (part 6). The first build takes a
few minutes; later ones take seconds.

```bash
cd voice-agent
```

```bash
cp .env.example .env
```

Put your keys in `.env`:

```
ISOQUANT_API_KEY=...        # only if you use the Isoquant LLM
OPENROUTER_API_KEY=...
ACCESS_TOKEN=...            # required on anything reachable from the internet
```

Start the server. The script downloads ONNX Runtime once (used for voice detection),
builds the program and runs it:

```bash
./scripts/start-server.sh
```

Open http://localhost:8080. The UI is served by the same process.

To check your keys and models without the UI, run the smoke test. It makes one real call
each to TTS, STT and LLM and prints PASS or FAIL with the provider's own error:

```bash
./target/release/voice-agent --smoke
```

```bash
./target/release/voice-agent --smoke openrouter-only
```

With only an OpenRouter key, set `active_preset: openrouter-only` in `config/default.yaml`
(or `VOICE_AGENT_PRESET=openrouter-only` in `.env`).

---

## 3. Use it

**Start a conversation.** Pick a mode on the start screen, then tap the round microphone
button and talk. Pause and it answers. Talk over it to interrupt. You can also type.

| Feature | Where | What it does |
|---|---|---|
| Modes | Start screen | English practice, Jokes and fun, Anything, plus your own ("Create your own") |
| Saved conversations | Left sidebar | Click to resume; the assistant remembers. Rename, delete, search |
| Folders | Left sidebar, "+ Folder" | Drag a conversation onto a folder, or use its move button (⇄). Rename, delete, reorder by dragging, open and close. Deleting a folder keeps its conversations |
| Layout | Edges of the side panels | Drag an edge to resize, double-click to reset, « hides the sidebar. Settings sections open and close. All remembered per browser |
| Replay | "Play" under a message | Hear your own recording or the reply again |
| Feedback | "Feedback" button (English practice) | Mistakes with corrections, level, what to practise |
| Instructions | "Instructions" button | Rules for every conversation, and extra rules for the open one |
| Cost | Amount in the top bar, Details, Cost | This turn, this conversation, this visit |
| Logs | Details, Logs | One line per model call, with timing and cost; errors in red |
| Microphone | Details, Settings | Choose device, sensitivity, run a 4-second test |
| Voice | Details, Settings | Voice, speaking speed, how long a pause ends your turn |
| Models | Details, Settings | Preset, or each model separately, with its price |
| Theme | Details, Settings | System, Light, Dark, Midnight, Forest, Sunset, Rose |
| Export | Details, Settings | Conversation as text, recording as WAV, debug log as JSON |

**If it cuts you off while you think:** Settings, "How long a pause ends your turn", choose
Patient.

**If it reacts to background noise or does not hear you:** Settings, run "Test microphone",
then change Sensitivity.

**If it interrupts itself:** your speakers are reaching the microphone. Use headphones, or
set `vad.barge_in: false` in `config/default.yaml`.

---

## 4. Models and prices

### The catalog

Defined in `config/models.yaml`, priced in `config/pricing.yaml`. Every OpenRouter model
below passed a live test call on 2026-11-16. "Speed" is time to the first word (LLM), time
to transcribe a 5-second clip (STT) or time to first audio (TTS), measured once or twice
from a cloud server; expect variation. Prices are USD and change: re-check them (see
"Check current prices").

**Language models**

| Id | Model | Input / output per 1M tokens | Speed | Notes |
|---|---|---|---|---|
| `isoquant-deepseek-fast` | DeepSeek V4.1 Flash Fast (Isoquant) | $0.035 / $0.37 | not measured | The project's chosen LLM. Price from the project plan, not verified |
| `deepseek-flash` | deepseek/deepseek-v4.1-flash | $0.14 / $0.42 | 0.3 s | Good default on OpenRouter |
| `deepseek-flash-1017` | deepseek/deepseek-v4.1-flash-1017 | $0.30 / $1.20 | 0.4 s | Peak price; half off-peak |
| `gemma-4-31b` | google/gemma-4-31b-it | $0.20 / $0.40 | 0.2 s | Fastest tested |
| `claude-haiku` | anthropic/claude-haiku-5.5 | $0.10 / $0.50 | 0.4 s | |
| `gemini-flash-lite` | google/gemini-3.1-flash-lite | $0.25 / $1.50 | 0.45 s | |
| `mistral-small` | mistralai/mistral-small-2603 | $0.15 / $0.60 | 0.4 s | |
| `ministral-8b` | mistralai/ministral-8b-2512 | $0.15 / $0.15 | 0.35 s | Cheapest output |
| `gpt-nano` | openai/gpt-5.4-nano | $0.20 / $1.25 | 0.8 s | |

**Speech to text**

| Id | Model | Per minute of audio | Speed |
|---|---|---|---|
| `whisper-turbo` | openai/whisper-large-v3-turbo | $0.0002 | 0.45 s |
| `whisper-large-v3` | openai/whisper-large-v3 | $0.00045 | 1.6 s |
| `nemotron-transcribe` | nvidia/nemotron-3.5-transcribe | $0.001 | 0.4 s |
| `gpt-4o-mini-transcribe` | openai/gpt-4o-mini-transcribe | about $0.002 | 0.55 s |
| `gpt-4o-transcribe` | openai/gpt-4o-transcribe | about $0.004 | 0.5 s |
| `whisper-1` | openai/whisper-1 | $0.006 | 1.6 s |

**Text to speech**

| Id | Model | Per 1,000 characters | Speed |
|---|---|---|---|
| `kokoro` | hexgrad/kokoro-82m | $0.00062 | 0.4 s |
| `orpheus` | canopylabs/orpheus-3b-0.1-ft | $0.007 | 0.4 s |
| `grok-voice` | x-ai/grok-voice-tts-1.0 | $0.015 | 0.25 s |
| `mai-voice-flash` | microsoft/mai-voice-2.1-flash | $0.015 | 0.3 s |
| `aura-2` | deepgram/aura-2 | $0.03 | 0.35 s |
| `eleven-flash` | elevenlabs/eleven-flash-v2.5 | $0.04 | 0.3 s |

**What a turn costs.** A typical turn is about 5 seconds of your speech, 150 input and 40
output tokens, and 120 characters spoken. With the `openrouter-only` preset that measured
about $0.0001 per turn. Text to speech is usually the largest part: the same turn with
`grok-voice` costs about $0.002, twenty times more, almost all of it the voice.

**Presets**

| Preset | LLM | STT | TTS |
|---|---|---|---|
| `default` | isoquant-deepseek-fast | whisper-turbo | kokoro |
| `premium-voice` | isoquant-deepseek-fast | whisper-turbo | grok-voice |
| `openrouter-only` | deepseek-flash-1017 | whisper-turbo | kokoro |
| `fastest` | gemma-4-31b | nemotron-transcribe | grok-voice |
| `cheapest` | ministral-8b | whisper-turbo | kokoro |

### Change the model

Three ways, from quickest to most permanent:

1. **In the UI, any time.** Details, Settings, Models. Pick a preset, or change the language
   model, speech-to-text or text-to-speech separately. Each shows its price. It applies to
   your next message and is remembered in that browser.
2. **Default for everyone.** One line in `config/default.yaml`:

   ```yaml
   active_preset: fastest
   ```

   or the environment variable `VOICE_AGENT_PRESET=fastest`. Config files are re-read when a
   page loads, so no restart is needed for YAML edits.
3. **New preset.** Add three lines under `presets:` in `config/models.yaml`.

### Add another OpenRouter model

1. Find its id and price.

   ```bash
   curl -s "https://openrouter.ai/api/v1/models" | python3 -m json.tool | less
   ```

   ```bash
   curl -s "https://openrouter.ai/api/v1/models?output_modalities=speech" | python3 -m json.tool | less
   ```

   ```bash
   curl -s "https://openrouter.ai/api/v1/models?output_modalities=transcription" | python3 -m json.tool | less
   ```

   Prices in that list are per token (LLM), per second (most STT) or per character (TTS).
   Multiply by 1,000,000, by 60, or by 1,000 to get the units `pricing.yaml` uses.
2. Add an entry under the right kind in `catalog:` in `config/models.yaml`:

   ```yaml
       my-new-model:
         label: Name shown in the UI
         provider: openrouter
         model: vendor/model-id
         max_tokens: 400
         extra: {reasoning: {enabled: false}}
   ```

   For an LLM, keep `reasoning` off: a thinking model makes you wait in silence. Some models
   refuse to turn it off ("Reasoning is mandatory"); those are a poor fit for voice.
   For TTS, add `voice:` and `voices: [...]` from the model's `supported_voices`.
3. Add its price under `openrouter:` in `config/pricing.yaml`.
4. Verify:

   ```bash
   ./scripts/check.sh
   ```

   Then reload the page, select the model in Settings, type one message and read the Logs
   tab. Or test without the UI by putting the model in a preset and running
   `./target/release/voice-agent --smoke <preset>`.

**TTS models must return raw PCM** (`response_format: pcm`). Any sample rate is converted.
A model that only returns MP3 (MiniMax speech did in testing) will not work.

### Check current prices

```bash
curl -s "https://openrouter.ai/api/v1/models" | python3 -c "
import json, sys
want = sys.argv[1:]
for m in json.load(sys.stdin)['data']:
    if m['id'] in want: print(m['id'], m['pricing'])
" deepseek/deepseek-v4.1-flash google/gemma-4-31b-it
```

### How cost is calculated

- If the API reports a cost in its response, that is used; the log line ends `(api)`.
  OpenRouter does this for LLM and STT.
- Otherwise the rate in `pricing.yaml` is used; the log line ends `(pricing.yaml)`.
  TTS is always calculated this way.
- A model with no rate still works; the log warns and counts it as $0.
- Details in `docs/cost-tracking.md`.

---

## 5. Your own GPU

You can run any of the three stages on your own machine and add it to the app at any time.
The requirement is a server that speaks the OpenAI API for that stage:

| Stage | Endpoint the app calls | Examples of servers that offer it |
|---|---|---|
| LLM | `POST {base}/chat/completions` with `stream: true` | Ollama, vLLM, LM Studio, llama.cpp server |
| STT | `POST {base}/audio/transcriptions` (multipart WAV upload, returns `{"text": ...}`) | Speaches (faster-whisper server) |
| TTS | `POST {base}/audio/speech` (returns PCM or WAV) | Kokoro-FastAPI |

Those server names are examples, not tested here; check each project's own instructions.
What was tested is the app's side, against a stand-in server that behaved like one: no API
key, no token counts in the reply, WAV and 22 kHz audio. All of that worked.

### Step 1: make the endpoint reachable

The **voice agent's server** calls your GPU machine, not your browser. So the address must
work from wherever the server runs.

| Where the voice agent runs | Address to use | Notes |
|---|---|---|
| Same machine as the GPU | `http://127.0.0.1:<port>/v1` | Simplest and fastest |
| Same home network | `http://192.168.x.x:<port>/v1` | Make the model server listen on all interfaces, not only localhost |
| In Docker on the GPU machine | `http://host.docker.internal:<port>/v1` | On Linux add `extra_hosts: ["host.docker.internal:host-gateway"]` to the compose service |
| In the cloud, GPU at home | A private tunnel address | Use a VPN-style tunnel (Tailscale, WireGuard) or an authenticated tunnel (Cloudflare Tunnel). Do not open the port to the internet without a key |

Check it from the machine that runs the voice agent:

```bash
curl -s http://192.168.1.50:11434/v1/models
```

### Step 2: add it in the UI

Details, Settings, "Add your own model…":

| Field | Example |
|---|---|
| Type | Language model |
| Name | My RTX: Llama 3.1 |
| Address | `http://192.168.1.50:11434/v1` |
| Model name | exactly what your server calls it, e.g. `llama3.1:8b` |
| API key | empty if the server needs none |
| Voices (TTS only) | `af_heart, am_adam` |
| Audio format (TTS only) | Raw PCM if the server streams it, otherwise WAV |
| Price | 0 for your own hardware |

Press "Add and test". The app makes one small real call and tells you in plain words whether
it worked ("could not connect", "HTTP 404 ... check the model name", or "works ... in 28 ms").
The model is selected immediately, appears in the model list as "Yours: ...", and can be
tested again or removed from the same panel. You can mix: your own LLM with cloud STT and
TTS, or all three local.

### Or add it in YAML

For a fixed setup you want in version control: uncomment `my-gpu` in
`config/providers.yaml`, add a catalog entry with `provider: my-gpu` in `config/models.yaml`,
and a zero price in `config/pricing.yaml`:

```yaml
my-gpu:
  llama3.1:8b: {input_per_million: 0, output_per_million: 0}
```

### Things to know about local models

- **Token counts.** Many local servers do not report usage. The app then estimates tokens at
  about four characters each and the log says `(tokens estimated)`. With a price of 0 this
  does not matter.
- **Speed.** A local LLM should start answering in well under a second to feel natural.
  Check "first token" in the Logs tab. A smaller or more quantised model is the usual fix.
- **Keep the model loaded.** The first call after a model is loaded into GPU memory can take
  many seconds. Configure your server to keep it in memory.
- **Voice replies should be short.** `max_tokens` is 400 for models added in the UI.
- **Security.** Adding a model makes the voice agent's server call the address you type. On
  a deployment shared with people you do not fully trust, set
  `custom_models.enabled: false` in `config/default.yaml`. API keys you enter are stored on
  the server in `data/custom_models.json` and are never sent back to a browser.

### Local model troubleshooting

| Message | Meaning |
|---|---|
| could not connect to ... | Wrong address or port, server not running, firewall, or it listens only on localhost |
| HTTP 404 ... check the model name | The server does not have a model by that exact name, or the address is missing `/v1` |
| HTTP 401 | The server wants an API key |
| expected raw PCM audio but got 'audio/mpeg' | Set the model's audio format to WAV, or configure the server for PCM or WAV |
| could not read the WAV audio | The server returned something that is not 16-bit WAV |
| it returned an empty reply | The model produced nothing; often a chat-template problem on the server |

---

## 6. Deploy

The app is one container on one port. See part 9 for what was and was not tried.

### Environment variables

| Variable | Required | Purpose |
|---|---|---|
| `OPENROUTER_API_KEY` | For OpenRouter models | |
| `ISOQUANT_API_KEY` | For the Isoquant LLM | |
| `ACCESS_TOKEN` | Yes, on the internet | The sign-in secret. Without it the app is open to anyone |
| `VOICE_AGENT_PRESET` | No | Overrides `active_preset` |
| `PORT` | No | Default 8080 |
| `ALLOWED_ORIGINS` | Rarely | Public host name(s), comma separated, if a proxy rewrites the `Host` header |

Make a token:

```bash
openssl rand -base64 32
```

### Option A: Docker Compose on a server

```bash
cd voice-agent
```

```bash
cp .env.example .env
```

Fill in `.env`, then:

```bash
docker compose up -d --build
```

```bash
curl -s http://localhost:8080/healthz
```

Data lives in the named volumes `voice-data` and `voice-logs`, which survive rebuilds.

### HTTPS is required

Browsers only allow the microphone on `https://` or `localhost`. Put a proxy that handles
TLS in front of port 8080. It must pass WebSocket connections on `/ws`. With Caddy, a
complete configuration is:

```
voice.example.com {
    reverse_proxy localhost:8080
}
```

For nginx, include the WebSocket upgrade headers (`Upgrade` and `Connection`) and
`proxy_read_timeout` of several minutes. If the page loads but stays on "Connecting…", the
proxy is blocking WebSockets or rewriting `Host`; see `ALLOWED_ORIGINS`.

### Option B: a platform that runs containers (Render, Railway, Fly.io and similar)

1. Point it at the repository with root directory `voice-agent` and the Dockerfile there.
2. Set the environment variables from the table above.
3. Attach a persistent disk mounted at `/app/data`. Without it, conversations and
   recordings are lost on every redeploy.
4. Health check path: `/healthz`.
5. One instance only. State is on local disk and connections are held in memory, so more
   than one instance would give each visitor a different half of the data.

### Option C: on your GPU machine at home

Run it with Docker or directly (part 2) on the same machine as your models, use
`http://127.0.0.1:<port>/v1` addresses, and reach the UI over a private tunnel that gives
you an `https://` address.

### Before going live

- [ ] `ACCESS_TOKEN` set to a long random value
- [ ] Served over HTTPS
- [ ] `/healthz` returns `"missing_keys": []`
- [ ] One full voice turn works from a phone and a laptop
- [ ] Persistent storage attached to `/app/data` and a backup planned
- [ ] Spending limits set in the OpenRouter and Isoquant dashboards
- [ ] Anyone else who will use it knows it records audio
- [ ] `docs/security.md` read

### Back up and restore

```bash
docker run --rm -v voice-agent_voice-data:/d -v "$PWD":/b alpine tar czf /b/voice-data.tgz -C /d .
```

```bash
docker run --rm -v voice-agent_voice-data:/d -v "$PWD":/b alpine tar xzf /b/voice-data.tgz -C /d
```

Without Docker, copy the `data/` folder. Restart the server after restoring: it keeps the
conversation list in memory.

### Update

```bash
git pull
```

```bash
docker compose up -d --build
```

Open pages reconnect by themselves and return to the same conversation.

---

## 7. Operate

**Health.** `GET /healthz` returns the active preset and any missing API keys.

**Logs.**

| Where | What |
|---|---|
| Details, Logs in the UI | Every model call for your visit, with timing and cost |
| Server output (`docker compose logs -f`) | The same lines for all visitors, plus start-up warnings |
| `logs/sessions/<session>.json` | Full record of one visit, including every turn's numbers |

A healthy turn looks like this:

```
STT Whisper...: 4.7s audio, waited 70 ms (started early during the pause), $0.000016 (api)
LLM ...: 258 in / 25 out tokens, first token 981 ms, done 1090 ms, $0.000015 (api)
TTS ...: 43 chars, first audio 547 ms, done 700 ms, $0.000027 (pricing.yaml)
Turn 3 done: ... = $0.000082 | session $0.000449 | first audio after 2454 ms
```

**Common failures.**

| Symptom | Cause | Fix |
|---|---|---|
| Red banner "`..._API_KEY is not set`" | Key missing on the server | Add it to `.env` or the platform, restart |
| "HTTP 401 ... check the API key" | Wrong or revoked key | Replace the key |
| "HTTP 402 ... out of credits" | Provider account empty | Top up |
| "HTTP 404 ... check the model name" | Model id wrong or withdrawn | Fix `models.yaml`, or pick another model in Settings |
| "HTTP 429" | Rate limited | Wait, or switch model |
| Microphone button gives an error | Not HTTPS, or permission denied | Use HTTPS; allow the microphone for the site |
| Stays on "Connecting…" | Proxy blocks WebSockets or rewrites `Host` | Fix the proxy; set `ALLOWED_ORIGINS` |
| Asks for the token again and again | Cookie not stored: plain HTTP on a non-local address, or third-party cookie blocking in an embedded frame | Use HTTPS, open the app directly |
| Replies are slow | See which line in Logs is slow | LLM: pick a faster model. TTS: `grok-voice`. Pause: Settings. `docs/latency.md` |
| "Session budget reached" | Per-visit cap hit | Reload, or raise `cost.session_budget_usd` |
| "Recording paused: ... MB limit" | Recordings reached `recording.max_total_mb` | Delete old conversations or raise the limit |
| Conversations vanished after a deploy | No persistent disk on `/app/data` | Attach one; restore from backup |

**Settings you are most likely to change** (`config/default.yaml`):

| Setting | Default | Effect |
|---|---|---|
| `active_preset` | `default` | Models used at start |
| `vad.min_silence_ms` | 550 | Pause that ends a turn. Lower is faster, higher is more patient |
| `vad.threshold` | 0.5 | Microphone sensitivity. Higher ignores more noise |
| `vad.barge_in` | true | Whether speaking interrupts the assistant |
| `assistant.max_history_turns` | 20 | Past exchanges sent to the LLM. Lower is cheaper and faster |
| `cost.session_budget_usd` | 0.50 | Spending cap per page visit; 0 disables |
| `recording.enabled` | true | Save audio |
| `recording.max_total_mb` | 2000 | Stop recording at this total size |
| `custom_models.enabled` | true | Allow adding model endpoints in the UI |
| `server.max_connections` | 20 | Browser tabs connected at once |

Modes (prompts, greetings, per-mode speed and pause) are in `config/modes.yaml`.

---

## 8. For AI agents

Read this part before changing anything. It applies to any coding agent working in this
folder.

### Ground rules

1. **Never commit secrets.** Keys live in `.env` (gitignored). Never print a key, write one
   into YAML, a test, a log or a commit message. If a key appears in a chat or a file, tell
   the user to rotate it.
2. **Never commit user data.** `data/` and `logs/sessions/` are gitignored; keep them so.
3. **Do not claim something works unless you ran it.** Say what you ran and what you saw.
   If you could not test (no key, no microphone, no GPU), say so in those words and record
   it in `docs/checkpoints.md`.
4. **Real calls cost money.** Unit tests and `scripts/check.sh` are free. `--smoke`,
   the UI and "Test" buttons make paid calls (fractions of a cent). Do not loop them.
5. **Configuration belongs in YAML, not code.** Models, prices, prompts and thresholds are
   in `config/`. Do not hard-code a model name or a price in Rust or JavaScript.
6. **Keep the UI free of HTML injection.** Insert text with `textContent`. Never build HTML
   from conversation text, model names or anything a user typed. No inline `<script>` or
   `style=` attributes: the Content-Security-Policy blocks them.
7. **Errors must be readable.** Every failure a user can hit should produce one sentence
   that says what failed and what to do. Raise `ProviderError` or `ConfigError`.
8. **Do not weaken security to make something work.** No disabling the origin check, the
   access token, TLS verification or the CSP. If one blocks you, report it.
9. **This folder only.** The repository root holds a different project. Leave it alone.

### File map

| Path | Responsibility | Touch it when |
|---|---|---|
| `config/default.yaml` | Active preset, VAD, budget, recording, limits | Changing defaults |
| `config/models.yaml` | Model catalog and presets | Adding or removing models |
| `config/pricing.yaml` | Rates | Adding a model, updating prices |
| `config/providers.yaml` | Provider base URLs and key variable names | Adding a provider |
| `config/modes.yaml` | Conversation modes | Changing assistant behaviour |
| `src/config.rs` | Loads and validates all YAML; `Stage`, `price_text` | Config format changes |
| `src/providers.rs` | The three HTTP calls; error messages; audio conversion | Provider quirks |
| `src/session.rs` | One connection: turn pipeline, cost, modes, models, instructions, feedback, folders | Most behaviour changes |
| `src/store.rs` | Files under `data/`: conversations, audio, folders, custom modes and models, instructions | Persistence |
| `src/vad.rs` | Voice detection (Silero model through ONNX Runtime) and turn detection | Rarely |
| `src/pricing.rs` | Cost arithmetic | Rarely |
| `src/main.rs` | HTTP and WebSocket endpoints, sign-in, security headers, message dispatch | New endpoints or messages |
| `src/cli.rs` | `--check` and `--smoke` | New checks |
| `assets/silero_vad.onnx` | The voice-detection model (2 MB) | Never |
| `Cargo.toml`, `Cargo.lock` | Rust dependencies | Adding a dependency |
| `ui/index.html`, `ui/style.css`, `ui/app.js` | The whole UI, no build step | UI changes |
| `ui/theme.js` | Theme applied before first paint | Themes |
| `ui/mic-worklet.js` | Microphone capture and resampling | Rarely |
| `scripts/check.sh` | Free checks | Run before every commit |
| `scripts/get-onnxruntime.sh` | Downloads ONNX Runtime into `vendor/` | Once per machine |
| `voice-agent --smoke [preset]` | One paid call per stage | After model or provider changes |
| `#[cfg(test)]` blocks in `src/*.rs` | Unit tests (`cargo test --release`) | Add a test with each logic change |
| `docs/` | Cost, latency, security, models, checkpoints | Keep in step with the code |

### WebSocket protocol

Browser to server: binary frames are 16 kHz mono 16-bit PCM microphone audio. JSON frames:

`text`, `new`, `open`, `close`, `delete`, `rename`, `move`, `folder_create`, `folder_rename`,
`folder_delete`, `folder_order`, `settings`, `set_preset`, `set_models`,
`add_model`, `delete_model`, `test_model`, `set_instructions`, `review`, `save_mode`,
`delete_mode`, `raw`, `stop`.

Server to browser: binary frames are 24 kHz mono 16-bit PCM reply audio. JSON frames:

`hello` (everything the UI needs: models, catalog, modes, voices), `conversations`,
`conversation`, `conversation_meta`, `status`, `transcript`, `cost`, `log`, `raw`, `error`,
`audio_start`, `audio_end`, `interrupt`, `review`, `instructions`, `model_test`.

Handlers are in `src/main.rs` (`handle`) and `ui/app.js` (`onMessage`). A new message
needs both sides.

### How to verify a change

Always:

```bash
./scripts/check.sh
```

If you touched models, providers or pricing, and a key is available:

```bash
./target/release/voice-agent --smoke openrouter-only
```

If you touched the server or the UI, run it and exercise the change in a real browser:

```bash
VOICE_AGENT_PRESET=openrouter-only ./scripts/start-server.sh
```

A headless browser can stand in for a person. Chromium flags
`--use-fake-ui-for-media-stream --use-fake-device-for-media-stream
--use-file-for-fake-audio-capture=<speech.wav>` feed a recording in as the microphone. Check
that the browser console has no errors, that the Logs tab shows the turn, and that the page
does not scroll sideways at 390 px width.

Typed messages go through the same pipeline as speech minus STT, so they are the cheapest
way to test LLM, TTS, cost and storage.

### Recipes

**Add a cloud model.** Part 4, "Add another OpenRouter model". Then update the tables in
part 4 of this playbook with the price and a measured speed.

**Add a provider.** Add it to `config/providers.yaml` with `base_url` and `api_key_env`, add
the variable to `.env.example`, add catalog entries and prices. If its API differs from the
OpenAI shape, adapt it in `src/providers.rs` only.

**Add or change a mode.** Edit `config/modes.yaml`. Fields are documented at the top of that
file. Reload the page. Prompts must tell the model its reply is spoken: short, no markdown.

**Update prices.** Run the price command in part 4, edit `config/pricing.yaml`, update the
date in its header and the tables in part 4.

**Debug "it does not answer".** In order: `/healthz` for missing keys; the Logs tab for a
red line; the server output for an error; `./target/release/voice-agent --smoke <preset>` to
see which stage fails; the model's "Test" button for a custom model.

**Debug "it is slow".** Read one turn in the Logs tab. "waited N ms" is STT, "first token"
is the LLM, "first audio" is TTS. Change the slowest stage's model. `docs/latency.md`.

**Add a setting.** Default in `config/default.yaml` with a comment; read it in
`Session::new`; if users may override it, send it in the `settings` message from
`sendSettings()` in `ui/app.js` and apply it in `Session::apply_settings`.

**Deploy.** Part 6. Do not deploy, push to a protected branch, or change a live environment
unless the user asked for exactly that.

### Definition of done

- `./scripts/check.sh` passes.
- The change was exercised for real, or the report says precisely what was not.
- Docs that describe the changed behaviour are updated, including this playbook.
- No secrets, no user data and no debugging leftovers in the commit.
- The summary to the user leads with what now works and what does not.

---

## 9. What is not verified

As of 2026-11-16:

- **The Isoquant LLM.** No Isoquant key was available. The endpoint answers and rejects a
  missing key correctly, but no turn has run through it. Its price is from the project plan.
  First thing to do with a key: `./target/release/voice-agent --smoke default`.
- **A real microphone and real speakers.** Voice tests used recordings fed to a simulated
  microphone. Echo, interruption with loudspeakers, sensitivity in a noisy room and speaker
  selection are untested.
- **Real local model servers.** The custom endpoint feature was tested against a stand-in
  server, not against Ollama, vLLM, Speaches or Kokoro-FastAPI themselves.
- **A real deployment.** The Docker image was built (187 MB) and a real turn ran inside the
  container. `docker compose`, HTTPS proxies, tunnels and specific hosting platforms were
  not tried. Only Linux x86-64 was built and run; macOS, Windows and ARM were not.
- **Restoring data under a running server.** The server keeps the conversation list in
  memory. After copying files into `data/` by hand, restart it.
- **Speeds and prices** were measured once or twice on one day and will drift.

`docs/checkpoints.md` has the full list of what was checked and how.
