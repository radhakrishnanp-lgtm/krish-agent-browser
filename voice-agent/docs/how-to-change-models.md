# How to change models

## Switch to another preset

Change one line in `config/default.yaml`:

```yaml
active_preset: premium-voice
```

or set `VOICE_AGENT_PRESET=premium-voice` in the environment (this wins over the file).

Config files are re-read whenever a browser session starts, so a page reload is enough; no
restart is needed. The **Preset** dropdown in the status bar switches for the current session only.

## Change one model

Edit the preset in `config/models.yaml`, for example a different voice model:

```yaml
    tts:
      provider: openrouter
      model: x-ai/grok-voice-tts-1.0
      voice: eve
```

Then add a rate for it in `config/pricing.yaml`. If a model has no rate, the app still works,
logs a warning, and counts that stage as $0.

Options per stage:

| Stage | Options |
|---|---|
| `llm` | `max_tokens`, `temperature`, `extra` (merged into the request body as-is) |
| `stt` | `language` (optional ISO code, e.g. `en`) |
| `tts` | `voice` (default voice), `voices` (list offered in the UI voice picker), `speed` |

The TTS model must support `response_format: pcm` (24 kHz, 16-bit, mono). If it does not, the
log panel shows "expected PCM audio but got ...".

## Find model ids on OpenRouter

```bash
curl -s "https://openrouter.ai/api/v1/models?output_modalities=speech"         # TTS models, with voices and prices
```

```bash
curl -s "https://openrouter.ai/api/v1/models?output_modalities=transcription"  # STT models
```

## Add a provider

Add it to `config/providers.yaml` with its OpenAI-compatible base URL and the name of the env
var holding its key, put the key in `.env`, then reference the provider name from a preset.

## Check a change

```bash
python3 scripts/smoke_test.py <preset-name>
```

It calls TTS, STT and LLM once each and prints PASS or FAIL with the provider's error message.

## Change how the assistant behaves

Modes are in `config/modes.yaml`. Each has a `system_prompt`, an optional spoken `greeting`,
an optional `tts_speed` (English practice speaks at 0.9x), `stt_language`, `min_silence_ms`
(English practice waits 0.8 s so learners have time to think) and `review_prompt` (adds the
Feedback button).
Copy a block to add a mode; it appears on the start screen after a page reload.

Users can also create modes from the start screen ("Create your own"). Those are stored in
`data/custom_modes.json`, not in the YAML.
