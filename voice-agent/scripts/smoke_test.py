#!/usr/bin/env python3
"""Calls each stage of the active preset once and prints the result and cost.

Run from the voice-agent folder:  python3 scripts/smoke_test.py [preset]
Order: TTS (makes audio) -> STT (transcribes that audio) -> LLM.
"""
import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from server import pricing, providers  # noqa: E402
from server.config import Config, ConfigError  # noqa: E402


async def main() -> int:
    cfg = Config()
    preset = sys.argv[1] if len(sys.argv) > 1 else cfg.active_preset
    stages = cfg.resolve(preset)
    print(f"Preset: {preset}")
    failed = 0
    pcm16k = b""

    try:
        text = "Hello, this is a smoke test."
        pcm = b"".join([c async for c in providers.synthesize(stages["tts"], text)])
        cost, src = pricing.tts_cost(cfg.rates(stages["tts"]), len(text))
        print(f"PASS  TTS  {stages['tts'].model}: {len(pcm) / 2 / providers.TTS_SAMPLE_RATE:.1f}s audio, ${cost:.6f} ({src})")
        import numpy as np
        a = np.frombuffer(pcm, dtype=np.int16)
        pcm16k = np.interp(np.arange(0, len(a), 1.5), np.arange(len(a)), a).astype(np.int16).tobytes()
    except (providers.ProviderError, ConfigError) as e:
        failed += 1
        print(f"FAIL  TTS  {e}")

    if pcm16k:
        try:
            r = await providers.transcribe(stages["stt"], pcm16k, 16000)
            cost, src = pricing.stt_cost(cfg.rates(stages["stt"]), r["seconds"], r["api_cost"])
            print(f"PASS  STT  {stages['stt'].model}: \"{r['text']}\", ${cost:.6f} ({src})")
        except (providers.ProviderError, ConfigError) as e:
            failed += 1
            print(f"FAIL  STT  {e}")
    else:
        print("SKIP  STT  (no audio because TTS failed)")

    try:
        reply, usage = "", None
        async for c in providers.chat_stream(stages["llm"], [{"role": "user", "content": "Say hello in five words."}]):
            if "delta" in c:
                reply += c["delta"]
            else:
                usage = c["usage"]
        u = usage or {}
        cost, src = pricing.llm_cost(cfg.rates(stages["llm"]), u.get("prompt_tokens", 0), u.get("completion_tokens", 0), u.get("cost"))
        note = "" if usage else "  WARNING: provider sent no usage block; the app will estimate tokens"
        print(f"PASS  LLM  {stages['llm'].model}: \"{reply.strip()}\" in={u.get('prompt_tokens')} out={u.get('completion_tokens')}, ${cost:.6f} ({src}){note}")
    except (providers.ProviderError, ConfigError) as e:
        failed += 1
        print(f"FAIL  LLM  {e}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
