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

## Fifth round: model catalog, your own endpoints, playbook

| Feature | Status | How it was checked |
|---|---|---|
| 20 OpenRouter models in the catalog (8 LLM, 6 STT, 6 TTS) | Verified | One live test call each; all passed |
| Switch any single model in the UI; remembered per browser | Verified | Headless browser; the turn log showed the new model |
| Add your own endpoint (LLM, STT, TTS), test, remove | Verified against a stand-in server | No API key, no usage block, WAV and 22 kHz PCM audio all worked. Not tried against Ollama, vLLM, Speaches or Kokoro-FastAPI |
| Clear error for a wrong address | Verified | "could not connect to ..." |
| TTS at sample rates other than 24 kHz | Verified | 44.1 kHz and 22 kHz are converted; a 440 Hz tone stayed at 440 Hz |
| Docker image (earlier Python version, since replaced) | Verified | Built (465 MB), started, served the UI, signed in, wrote to the data volume. The build here needed this sandbox's proxy certificate, which a normal machine does not |
| `docker compose up` | Not tested | |

## Sixth round: folders, flexible layout, Rust server

| Feature | Status | How it was checked |
|---|---|---|
| Folders: create, rename, delete, reorder, collapse | Verified | Headless browser, on both the old and the new server |
| Move a conversation by drag and drop, and by the move dialog | Verified | Drag onto a folder, drag out to "Not in a folder", dialog path |
| Deleting a folder keeps its conversations | Verified | Three conversations still listed afterwards |
| Resize sidebar and details by dragging; collapse the sidebar; remembered after reload | Verified | Widths 400 and 500 px kept; sidebar stayed hidden after reload |
| Settings sections open and close | Verified | Closed section hid its controls |
| Server rewritten in Rust with the same behaviour | Verified | The whole earlier test battery was re-run against it: conversations and resume, barge-in, all 20 OpenRouter models, custom endpoints, 12 path-traversal attempts, sign-in, origin check, limits, recordings, feedback, modes, themes, microphone test, instructions, reconnect |
| Rust unit tests | Verified | 17 pass (`cargo test --release`) |
| Docker image (Rust) | Verified | Built (187 MB), ran as a non-root user using 28 MB of memory, completed a real turn and saved it to a named volume. The build here needed this sandbox's proxy certificate, which a normal machine does not |
| ONNX Runtime versions | Verified with 1.23.2 (downloaded) and 1.29.0 | Voice detection output matched the Python reference to 8 decimal places |
| `docker compose up` | Not tested | |
| macOS, Windows, ARM | Not tested | Built and run on Linux x86-64 only |

## Not yet verified

- **The Isoquant LLM call.** No Isoquant key was available, so the `default` and `premium-voice`
  presets have not been run end to end. The Isoquant endpoint was reached and answered
  "invalid API key", so the URL is right. Run `./target/release/voice-agent --smoke default` once the
  key is in `.env`.
- **A real microphone and speakers.** Tests used recorded audio, so echo and barge-in behaviour
  with real speakers is untested.
- **Real local model servers and a real deployment.** See `PLAYBOOK.md`, part 9.

## Differences from the original plan

- **No Hugging Face speech-to-speech library.** Its STT and TTS stages are built around local
  models. A small purpose-built server gives the same pipeline with a much smaller install.
- **TTS model.** OpenRouter has no `openai/gpt-4o-mini-tts-2025-12-15`. The default is
  `hexgrad/kokoro-82m` (voice `af_heart`).
- **STT model.** The default is `openai/whisper-large-v3-turbo` instead of `openai/whisper-1`:
  about three times faster and thirty times cheaper in testing.
- **No `start-ui.sh`.** The server serves the UI.
