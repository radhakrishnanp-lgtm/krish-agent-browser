# How to change models

The full guide is in [PLAYBOOK.md](../PLAYBOOK.md): part 4 (the catalog, prices, switching,
adding a cloud model) and part 5 (your own GPU). This page is the quick reference.

| I want to | Do this |
|---|---|
| Try another model right now | UI: Details, Settings, Models. Applies to your next message |
| Change the default for everyone | `active_preset:` in `config/default.yaml`, or `VOICE_AGENT_PRESET` |
| Add a cloud model | Entry under `catalog:` in `config/models.yaml`, price in `config/pricing.yaml`, run `./scripts/check.sh` |
| Add a model on my own machine | UI: Details, Settings, "Add your own model…" |
| Make a new preset | Three lines under `presets:` in `config/models.yaml` |
| Change how the assistant behaves | `config/modes.yaml`, or "Create your own" on the start screen, or the Instructions button |

## File format

```yaml
catalog:
  llm:
    my-model:                       # the id used in presets and remembered by browsers
      label: Name shown in the UI
      provider: openrouter          # from config/providers.yaml
      model: vendor/model-id        # the provider's own name for it
      max_tokens: 400
      extra: {reasoning: {enabled: false}}   # merged into the request as-is
  stt:
    my-stt: {label: ..., provider: openrouter, model: vendor/stt-id, language: en}
  tts:
    my-tts: {label: ..., provider: openrouter, model: vendor/tts-id, voice: name, voices: [name, other]}

presets:
  my-preset: {llm: my-model, stt: my-stt, tts: my-tts}
```

Options per stage:

| Stage | Options |
|---|---|
| `llm` | `max_tokens`, `temperature`, `extra` |
| `stt` | `language` (optional ISO code) |
| `tts` | `voice`, `voices`, `speed`, `format` (`pcm`, the default, or `wav`), `sample_rate` (only if the server sends PCM without saying its rate) |

Config files are re-read when a page loads; no restart is needed. A mistake in the YAML is
reported in the page and in the server output with the file and the reason.

## Check a change

```bash
./scripts/check.sh
```

```bash
./target/release/voice-agent --smoke <preset-name>
```
