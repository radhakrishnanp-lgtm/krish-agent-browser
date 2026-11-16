# Latency

"Reply time" in the UI is measured from the moment you stop talking to the first sound of
the reply. It is shown under every reply and in Details, Cost.

## What was measured

With the all-OpenRouter preset on 2026-11-16, from a cloud server, using recorded speech:
reply times between 0.8 s and 2.6 s, mostly 1.1 to 1.5 s. The Isoquant LLM has not been
measured (no key was available), so the default preset may be faster or slower than this.

Where the time goes in a typical turn:

| Step | Typical | Notes |
|---|---|---|
| Waiting to be sure you finished | 550 ms | `vad.min_silence_ms` |
| Speech to text | about 0 ms extra | Starts during the pause (`vad.early_stt_ms`), usually done by the time the turn ends |
| LLM first sentence | 400 to 1000 ms | The most variable step |
| Text to speech, first audio | 250 to 900 ms | Depends on the voice model |

## What the app does to be fast

- **Early transcription.** After 250 ms of silence the audio is sent to STT, before the turn
  officially ends at 550 ms. If you keep talking, that result is discarded.
- **Sentence streaming.** The first sentence goes to TTS while the LLM is still writing the rest.
  A long first sentence is split at a comma.
- **Parallel speech synthesis.** Up to three sentences are synthesised at once and played in order.
- **Warm connections.** Connections to the providers are opened when you start speaking and
  kept alive, which saves about 200 to 300 ms per call.
- **Fast STT model.** `openai/whisper-large-v3-turbo` took 0.5 to 0.9 s on a 5 s clip;
  `openai/whisper-1` took about 2.1 s.

## Settings that trade speed for something else

| Setting | Faster | Cost of going faster |
|---|---|---|
| `vad.min_silence_ms` (default 550) | Lower, e.g. 400 | Cuts you off if you pause mid-sentence. Language learners may want 800 or more |
| Preset `premium-voice` | `x-ai/grok-voice-tts-1.0` started in about 240 ms warm, against 450 to 700 ms for the default voice | About 25 times the TTS price ($0.015 against $0.00062 per 1,000 characters) |
| `llm.max_tokens` in `models.yaml` | Lower | Shorter answers |
| Mode `system_prompt` | Ask for shorter replies | Less detail |

Run the server close to the providers (a US or EU cloud region) and close to you; network
distance adds to every step.

## Finding a slow step

Details, Logs shows one line per call:

```
STT ...: 4.7s audio, waited 70 ms (started early during the pause)
LLM ...: 258 in / 25 out tokens, first token 981 ms, done 1090 ms
TTS ...: 43 chars, first audio 547 ms, done 700 ms
Turn 3 done: ... | first audio after 2454 ms
```
