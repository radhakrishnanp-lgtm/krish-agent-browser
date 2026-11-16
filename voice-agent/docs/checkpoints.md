# Checkpoints

Status as of 2026-11-16. "Verified" means it was run and observed, with what was used to check it.

| # | Checkpoint | Status | How it was checked |
|---|---|---|---|
| 1 | Structure is clean and secrets are not committed | Verified | `.env` is gitignored; `git check-ignore .env` |
| 2 | Changing one preset name switches the model; pricing rates defined | Verified | Unit test checks every preset resolves and has rates; preset switched in the UI |
| 3 | Terminal shows model, tokens and cost after every reply | Verified | "Turn N done" line in the server log |
| 4 | Speak and get a voice reply, with cost | Verified with the `openrouter-only` preset | Recorded speech fed through the WebSocket and through a browser with a fake microphone |
| 5 | UI shows live cost and clear logs; errors are visible | Verified | Headless browser run: cost panel, log panel, red error banner for a missing key |
| 6 | Documented and easy to change | Done | This folder and the README |

## Not yet verified

- **The Isoquant LLM call.** No Isoquant key was available, so the `default` and `low-cost`
  presets have not been run end to end. The Isoquant endpoint was reached and answered
  "invalid API key", so the URL is right. Run `python3 scripts/smoke_test.py default` once the
  key is in `.env`.
- **A real microphone and speakers.** Tests used recorded audio, so echo and barge-in behaviour
  with real speakers is untested.
- **The Docker image.** The Dockerfile was written but not built.

## Differences from the original plan

- **No Hugging Face speech-to-speech library.** Its STT and TTS stages are built around local
  models. A small purpose-built server gives the same pipeline with a much smaller install.
- **TTS model.** OpenRouter has no `openai/gpt-4o-mini-tts-2025-12-15`. The default is
  `hexgrad/kokoro-82m` (voice `af_heart`).
- **No `start-ui.sh`.** The server serves the UI.
