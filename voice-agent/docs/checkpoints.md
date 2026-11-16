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

## Second round: conversations, modes, speed

| Feature | Status | How it was checked |
|---|---|---|
| Saved conversations: list, resume, rename, delete | Verified | WebSocket test and headless browser; history survived a page reload |
| The assistant remembers a resumed conversation | Verified | Asked it to repeat the first sentence after reconnecting |
| Modes: English practice, Jokes and fun, Anything | Verified | English mode corrected "I go to market and buyed three apple" and the UI showed the correction block |
| Early transcription, parallel TTS, warm connections | Verified | Log lines show "started early during the pause"; reply time 0.8 to 2.6 s |
| Barge-in | Verified with recorded audio | Reply stopped and a new turn started |
| Voice and speed settings, export, debug log download | Verified | Headless browser |

## Third round: recordings and practice tools

| Feature | Status | How it was checked |
|---|---|---|
| Recording both sides, Play button per message | Verified | Headless browser: recording played back; files present in `data/audio` |
| Download whole conversation as one WAV | Verified | Endpoint returned a 1.8 MB file for a five-message conversation |
| Recordings deleted with the conversation; recording can be switched off | Verified | Folder gone after delete; no new files with the toggle off |
| English feedback report | Verified | Five mistakes listed with corrections and a level, saved and shown again after reload |
| Create and delete your own mode | Verified | A "Pirate captain" mode answered in character |
| Pause-length setting, conversation search | Verified in the UI | Setting accepted; search filtered the list. The effect of pause length on real speech is untested |

## Fourth round: microphone tools, themes, instructions, security

| Feature | Status | How it was checked |
|---|---|---|
| Microphone test with playback | Verified with a simulated microphone | Reported "Sounds good (peak 84%)" and switched the microphone off again |
| Choose microphone and speaker | Microphone verified with simulated devices; speaker choice not verified | A voice turn worked on the second simulated microphone. No real audio output exists in the test browser |
| Sensitivity setting | Accepted and saved; a voice turn worked at Low | Its effect on real background noise is untested |
| Themes | Verified | All seven applied and the choice survived a reload |
| Custom instructions | Verified | The global instruction was followed in two conversations; the per-conversation one only in its own |
| Sign-in cookie, origin check, headers, limits | Verified | See `docs/security.md` |
| Auto-reconnect | Verified | Server stopped and restarted; the page reconnected to the same conversation and stayed signed in |

## Not yet verified

- **The Isoquant LLM call.** No Isoquant key was available, so the `default` and `premium-voice`
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
- **STT model.** The default is `openai/whisper-large-v3-turbo` instead of `openai/whisper-1`:
  about three times faster and thirty times cheaper in testing.
- **No `start-ui.sh`.** The server serves the UI.
