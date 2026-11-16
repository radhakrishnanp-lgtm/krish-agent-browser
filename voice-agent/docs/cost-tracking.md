# Cost tracking

All amounts are USD.

## How each stage is costed

| Stage | Measured | Cost comes from |
|---|---|---|
| LLM | Input and output tokens from the API's `usage` block | The API's own `usage.cost` if present, else `tokens x rate` from `pricing.yaml` |
| STT | Seconds of audio | The API's own `usage.cost` if present, else `seconds / 60 x per_minute` |
| TTS | Characters sent | Always `characters / 1000 x per_1k_characters` (the speech endpoint returns audio only) |

Every log line says which one was used: `(api)` or `(pricing.yaml)`. The per-turn record in the
session log has the same under `cost_sources`.

OpenRouter reports the cost on LLM and STT responses, so with OpenRouter models those numbers
are what you were billed. Whether Isoquant returns a `usage.cost` field has not been tested
(no Isoquant key was available during development). If it does not, the LLM cost is calculated
from the two Isoquant rates in `pricing.yaml`, which were taken from the project plan and should
be checked against Isoquant's price list.

If a provider sends no `usage` block at all, tokens are estimated at about 4 characters per token
and the log line says `(tokens estimated)`.

## Things that are easy to miss

- An interrupted reply still costs: the tokens generated and the sentences already sent to TTS
  are counted.
- Audio where no words were recognised still costs the STT call.
- Early transcription (`vad.early_stt_ms`) starts STT during a short pause. If you carry on
  talking, that call is thrown away but still billed; its cost is added to the turn's STT cost
  and the log line says "for discarded early transcriptions". With the default STT model this
  is about $0.00001 each.
- The spoken greeting of a new conversation costs a TTS call.
- A feedback report is one extra LLM call (about $0.0001 to $0.0005 in testing). It is added to
  the conversation's LLM cost.
- Replaying a recording is free: it plays the saved file.
- `openai/whisper-1` bills whole seconds, rounded up.
- OpenRouter's `deepseek/deepseek-v4.1-flash-1017` has peak and off-peak prices. `pricing.yaml`
  holds the peak rate, but the API-reported cost is used in practice.

## Where the numbers show up

- **Top bar:** total for the open conversation, across every visit.
- **Under each reply:** that turn's cost and how fast the reply started.
- **Details, Cost tab:** last turn and conversation, split by LLM / STT / TTS, the total since
  the page was opened, and the remaining budget.
- **Details, Logs tab and the terminal:** one line per call and a summary line per turn.
- **`data/conversations/<id>.json`:** the conversation with its running totals.
- **`logs/sessions/<session_id>.json`:** per-visit debug log, rewritten after every turn.
  Download it from Details, Settings, "Download debug log".

## Budget

`cost.session_budget_usd` in `config/default.yaml` caps one page visit (not one conversation). When the total
reaches it, new turns are refused with a clear error. Set it to `0` to disable. It limits one
session, not your account; use the provider dashboards for account-level limits.

## Session log format

```json
{
  "session_id": "2026-11-16-05-44-53-c8df7300",
  "start_time": "...",
  "preset": "default",
  "models": { "llm": "...", "stt": "...", "tts": "..." },
  "turns": [
    {
      "index": 1,
      "input": "voice",
      "user_text": "...",
      "assistant_text": "...",
      "llm_input_tokens": 61,
      "llm_output_tokens": 35,
      "llm_cost": 0.000019,
      "stt_seconds": 5.152,
      "stt_cost": 0.000017,
      "tts_characters": 138,
      "tts_cost": 0.000086,
      "turn_total": 0.000122,
      "cost_sources": { "stt": "api", "llm": "api", "tts": "pricing.yaml" },
      "latency_ms": { "stt": 1140, "llm_first_token": 400, "llm": 900, "tts_first_audio": 700, "total": 3100 },
      "interrupted": false,
      "errors": []
    }
  ],
  "cost_breakdown": { "llm": 0.000019, "stt": 0.000017, "tts": 0.000086 },
  "session_total_cost": 0.000122,
  "events": [ { "ts": "...", "level": "info", "message": "..." } ]
}
```

Session logs contain the full conversation text. They are gitignored.
