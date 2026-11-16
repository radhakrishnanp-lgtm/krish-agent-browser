# How to change models

## Switch to another preset

Change one line in `config/default.yaml`:

```yaml
active_preset: low-cost
```

or set `VOICE_AGENT_PRESET=low-cost` in the environment (this wins over the file).

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
| `tts` | `voice`, `speed` |

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
