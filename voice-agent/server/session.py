"""One voice conversation: turn pipeline (STT -> LLM -> TTS), cost tracking, session log."""
from __future__ import annotations

import asyncio
import json
import logging
import re
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

from . import pricing, providers
from .config import ROOT, Config, ConfigError
from .providers import ProviderError
from .vad import SAMPLE_RATE, TurnDetector

log = logging.getLogger("voice-agent")

SENTENCE_END = re.compile(r"(?<=[.!?。！？])\s+|\n+")
MAX_SAVED_EVENTS = 2000
MIN_TTS_CHARS = 12


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds")


class Session:
    def __init__(self, cfg: Config, outbox: asyncio.Queue):
        self.cfg = cfg
        self.outbox = outbox
        self.start_time = now_iso()
        self.session_id = datetime.now().strftime("%Y-%m-%d-%H-%M-%S") + "-" + uuid.uuid4().hex[:8]
        self.preset = cfg.active_preset
        self.stages = cfg.resolve(self.preset)
        self.history: list[dict] = []
        self.turns: list[dict] = []
        self.events: list[dict] = []
        self.totals = {"llm": 0.0, "stt": 0.0, "tts": 0.0, "total": 0.0}
        self.show_raw = False
        self.turn_task: asyncio.Task | None = None
        v = cfg.default.get("vad", {})
        self.barge_in = bool(v.get("barge_in", True))
        self.vad = TurnDetector(
            threshold=v.get("threshold", 0.5),
            min_silence_ms=v.get("min_silence_ms", 700),
            min_speech_ms=v.get("min_speech_ms", 250),
            pre_roll_ms=v.get("pre_roll_ms", 300),
            max_utterance_s=v.get("max_utterance_s", 30),
        )
        self.budget = float((cfg.default.get("cost") or {}).get("session_budget_usd") or 0)
        self.log_path = ROOT / (cfg.default.get("logging") or {}).get("sessions_dir", "logs/sessions") / f"{self.session_id}.json"

    # ---------- outbound helpers ----------

    def send(self, msg: dict) -> None:
        self.outbox.put_nowait(msg)

    def emit_log(self, level: str, message: str, data: dict | None = None) -> None:
        entry = {"type": "log", "ts": now_iso(), "level": level, "message": message}
        if data is not None:
            entry["data"] = data
        getattr(log, "warning" if level == "warn" else level, log.info)(f"[{self.session_id[-8:]}] {message}")
        self.events.append({k: v for k, v in entry.items() if k != "type"})
        del self.events[:-MAX_SAVED_EVENTS]
        self.send(entry)

    def emit_raw(self, stage: str, payload) -> None:
        if self.show_raw:
            self.send({"type": "raw", "ts": now_iso(), "stage": stage, "payload": payload})

    def models(self) -> dict:
        return {k: s.model for k, s in self.stages.items()}

    def hello(self) -> dict:
        return {
            "type": "hello",
            "session_id": self.session_id,
            "preset": self.preset,
            "presets": list(self.cfg.presets),
            "models": self.models(),
            "providers": {k: s.provider for k, s in self.stages.items()},
            "budget_usd": self.budget,
            "barge_in": self.barge_in,
            "input_sample_rate": SAMPLE_RATE,
            "output_sample_rate": providers.TTS_SAMPLE_RATE,
            "missing_keys": self.cfg.missing_keys(self.preset),
        }

    def emit_cost(self, turn: dict | None) -> None:
        self.send({
            "type": "cost",
            "turn": {k: turn[k] for k in ("index", "llm_cost", "stt_cost", "tts_cost", "turn_total",
                                           "llm_input_tokens", "llm_output_tokens", "stt_seconds", "tts_characters")} if turn else None,
            "session": {k: round(v, 8) for k, v in self.totals.items()},
            "budget_usd": self.budget,
        })

    # ---------- session log ----------

    def save(self) -> None:
        doc = {
            "session_id": self.session_id,
            "start_time": self.start_time,
            "updated_time": now_iso(),
            "preset": self.preset,
            "models": self.models(),
            "turns": self.turns,
            "cost_breakdown": {k: round(v, 8) for k, v in self.totals.items() if k != "total"},
            "session_total_cost": round(self.totals["total"], 8),
            "events": self.events,
        }
        try:
            self.log_path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.log_path.with_suffix(".tmp")
            tmp.write_text(json.dumps(doc, indent=2, ensure_ascii=False))
            tmp.replace(self.log_path)
        except OSError as e:
            log.error("Could not write session log %s: %s", self.log_path, e)

    # ---------- control messages ----------

    def set_preset(self, name: str) -> None:
        try:
            self.cfg.reload()  # pick up YAML edits without a restart
            self.stages = self.cfg.resolve(name)
        except ConfigError as e:
            self.emit_log("error", f"Could not switch preset: {e}")
            return
        self.preset = name
        self.send(self.hello())
        self.emit_log("info", f"Preset switched to '{name}'", self.models())
        missing = self.cfg.missing_keys(name)
        if missing:
            self.emit_log("error", f"Missing API key(s) for this preset: {', '.join(missing)}. Add them to voice-agent/.env and restart.")

    async def clear(self) -> None:
        await self.cancel_turn()
        self.history.clear()
        self.vad.reset()
        self.emit_log("info", "Conversation cleared (costs and the session log are kept)")

    async def cancel_turn(self) -> None:
        task = self.turn_task
        if task and not task.done():
            task.cancel()
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass

    # ---------- audio in ----------

    async def on_audio(self, pcm: bytes) -> None:
        for ev in self.vad.feed(pcm):
            kind = ev[0]
            busy = self.turn_task is not None and not self.turn_task.done()
            if kind == "speech_start":
                self.send({"type": "status", "state": "user_speaking"})
            elif kind == "speech_confirmed":
                if self.barge_in:
                    self.send({"type": "interrupt"})
                    if busy:
                        self.emit_log("info", "Barge-in: user spoke, stopping the assistant")
                        await self.cancel_turn()
            elif kind == "speech_cancel":
                self.send({"type": "status", "state": "thinking" if busy else "listening"})
            elif kind == "speech_end":
                if busy and not self.barge_in:
                    self.emit_log("warn", "Ignored speech while the assistant was replying (barge_in is off)")
                    continue
                self.start_turn(pcm=ev[1])

    def start_turn(self, pcm: bytes | None = None, text: str | None = None) -> None:
        previous = self.turn_task
        if previous and not previous.done():
            self.send({"type": "interrupt"})

        async def runner():
            if previous and not previous.done():
                previous.cancel()
                try:
                    await previous
                except (asyncio.CancelledError, Exception):
                    pass
            await self.run_turn(pcm, text)

        self.turn_task = asyncio.create_task(runner())

    # ---------- the turn pipeline ----------

    def _add_cost(self, turn: dict, kind: str, cost: float, source: str) -> None:
        turn[f"{kind}_cost"] = round(turn[f"{kind}_cost"] + cost, 8)
        turn["cost_sources"][kind] = source
        turn["turn_total"] = round(turn["llm_cost"] + turn["stt_cost"] + turn["tts_cost"], 8)
        self.totals[kind] += cost
        self.totals["total"] += cost
        if source == "no-rate":
            stage = self.stages[kind]
            self.emit_log("warn", f"No rate for {stage.provider} / {stage.model} in config/pricing.yaml; its cost is counted as $0")
        self.emit_cost(turn)

    async def run_turn(self, pcm: bytes | None, text: str | None) -> None:
        if self.budget and self.totals["total"] >= self.budget:
            self.emit_log("error", f"Session budget of ${self.budget:.2f} reached (spent ${self.totals['total']:.4f}). "
                                   "Raise cost.session_budget_usd in config/default.yaml or reload the page for a new session.")
            self.send({"type": "status", "state": "listening"})
            return

        turn = {
            "index": len(self.turns) + 1, "time": now_iso(), "input": "voice" if pcm else "text",
            "user_text": text or "", "assistant_text": "",
            "llm_input_tokens": 0, "llm_output_tokens": 0, "llm_cost": 0.0,
            "stt_seconds": 0.0, "stt_cost": 0.0,
            "tts_characters": 0, "tts_cost": 0.0,
            "turn_total": 0.0, "cost_sources": {}, "latency_ms": {}, "interrupted": False, "errors": [],
        }
        self.turns.append(turn)
        t0 = time.monotonic()
        try:
            if pcm:
                await self._stt(turn, pcm)
                if not turn["user_text"]:
                    self.emit_log("warn", "No words recognised in that audio; turn skipped")
                    return
            self.send({"type": "transcript", "role": "user", "turn": turn["index"], "text": turn["user_text"], "final": True})
            await self._llm_and_tts(turn)
        except asyncio.CancelledError:
            turn["interrupted"] = True
            raise
        except (ProviderError, ConfigError) as e:
            turn["errors"].append(str(e))
            self.emit_log("error", str(e), {"raw": e.raw} if isinstance(e, ProviderError) and e.raw else None)
            self.send({"type": "error", "message": str(e)})
        except Exception as e:  # unexpected bug: show it rather than hide it
            log.exception("Unexpected error in turn")
            turn["errors"].append(repr(e))
            self.emit_log("error", f"Unexpected server error: {e!r} (see the terminal for the traceback)")
            self.send({"type": "error", "message": f"Unexpected server error: {e!r}"})
        finally:
            turn["latency_ms"]["total"] = int((time.monotonic() - t0) * 1000)
            if turn["user_text"]:
                self.history.append({"role": "user", "content": turn["user_text"]})
                if turn["assistant_text"]:
                    self.history.append({"role": "assistant", "content": turn["assistant_text"]})
            self.send({"type": "transcript", "role": "assistant", "turn": turn["index"], "text": turn["assistant_text"],
                       "final": True, "interrupted": turn["interrupted"]})
            self.send({"type": "audio_end", "turn": turn["index"]})
            self.emit_log("info",
                f"Turn {turn['index']} done: llm={self.stages['llm'].model} in={turn['llm_input_tokens']} out={turn['llm_output_tokens']} "
                f"| llm ${turn['llm_cost']:.6f} + stt ${turn['stt_cost']:.6f} + tts ${turn['tts_cost']:.6f} = ${turn['turn_total']:.6f} "
                f"| session ${self.totals['total']:.6f}" + (" (interrupted)" if turn["interrupted"] else ""),
                {"turn": turn})
            self.emit_cost(turn)
            self.send({"type": "status", "state": "listening"})
            self.save()

    async def _stt(self, turn: dict, pcm: bytes) -> None:
        stage = self.stages["stt"]
        self.send({"type": "status", "state": "transcribing"})
        t = time.monotonic()
        res = await providers.transcribe(stage, pcm, SAMPLE_RATE)
        turn["latency_ms"]["stt"] = int((time.monotonic() - t) * 1000)
        turn["user_text"] = res["text"]
        turn["stt_seconds"] = round(res["seconds"], 3)
        self.emit_raw("stt", res["raw"])
        cost, source = pricing.stt_cost(self.cfg.rates(stage), res["seconds"], res["api_cost"])
        self.emit_log("info", f"STT {stage.model}: {res['seconds']:.1f}s audio in {turn['latency_ms']['stt']} ms, ${cost:.6f} ({source})")
        self._add_cost(turn, "stt", cost, source)

    async def _llm_and_tts(self, turn: dict) -> None:
        llm = self.stages["llm"]
        max_hist = int((self.cfg.default.get("assistant") or {}).get("max_history_turns", 12)) * 2
        messages = [{"role": "system", "content": (self.cfg.default.get("assistant") or {}).get("system_prompt", "").strip()}]
        messages += self.history[-max_hist:] + [{"role": "user", "content": turn["user_text"]}]

        sentences: asyncio.Queue = asyncio.Queue()
        tts_task = asyncio.create_task(self._tts_worker(turn, sentences))
        self.send({"type": "status", "state": "thinking"})
        t = time.monotonic()
        usage, pending, first, completed = None, "", True, False
        try:
            async for chunk in providers.chat_stream(llm, messages):
                if "delta" in chunk:
                    if first:
                        turn["latency_ms"]["llm_first_token"] = int((time.monotonic() - t) * 1000)
                        first = False
                    turn["assistant_text"] += chunk["delta"]
                    pending += chunk["delta"]
                    self.send({"type": "transcript", "role": "assistant", "turn": turn["index"], "text": turn["assistant_text"], "final": False})
                    # Send complete sentences to TTS as they arrive; very short ones
                    # ("Hi!") wait for the next sentence so they are not a request of their own.
                    cut = 0
                    for m in SENTENCE_END.finditer(pending):
                        if m.start() - cut >= MIN_TTS_CHARS:
                            sentences.put_nowait(pending[cut:m.start()].strip())
                            cut = m.end()
                    pending = pending[cut:]
                else:
                    usage = chunk["usage"]
                    self.emit_raw("llm", chunk["raw"])
            if pending.strip():
                sentences.put_nowait(pending.strip())
            completed = True
        finally:
            turn["latency_ms"]["llm"] = int((time.monotonic() - t) * 1000)
            # Bill even if interrupted mid-stream (tokens were generated), but not if the
            # call failed before producing anything.
            if usage is not None or turn["assistant_text"]:
                self._bill_llm(turn, llm, messages, usage)
            if not completed:
                tts_task.cancel()
        if not turn["assistant_text"].strip():
            tts_task.cancel()
            raise ProviderError(llm, "the model returned an empty reply")
        sentences.put_nowait(None)
        try:
            await tts_task
        except asyncio.CancelledError:
            tts_task.cancel()
            raise

    def _bill_llm(self, turn: dict, llm, messages: list[dict], usage: dict | None) -> None:
        estimated = usage is None
        if estimated:  # provider sent no usage block: rough estimate, ~4 chars per token
            usage = {"prompt_tokens": sum(len(m["content"]) for m in messages) // 4,
                     "completion_tokens": len(turn["assistant_text"]) // 4}
        turn["llm_input_tokens"] = int(usage.get("prompt_tokens") or 0)
        turn["llm_output_tokens"] = int(usage.get("completion_tokens") or 0)
        cost, source = pricing.llm_cost(self.cfg.rates(llm), turn["llm_input_tokens"], turn["llm_output_tokens"], usage.get("cost"))
        if estimated:
            source += " (tokens estimated)"
        self.emit_log("info", f"LLM {llm.model}: {turn['llm_input_tokens']} in / {turn['llm_output_tokens']} out tokens "
                              f"in {turn['latency_ms']['llm']} ms, ${cost:.6f} ({source})")
        self._add_cost(turn, "llm", cost, source)

    async def _tts_worker(self, turn: dict, sentences: asyncio.Queue) -> None:
        stage = self.stages["tts"]
        started = False
        while True:
            sentence = await sentences.get()
            if sentence is None:
                return
            t = time.monotonic()
            # Billed for the characters submitted, so count the cost before streaming.
            cost, source = pricing.tts_cost(self.cfg.rates(stage), len(sentence))
            turn["tts_characters"] += len(sentence)
            self._add_cost(turn, "tts", cost, source)
            async for audio in providers.synthesize(stage, sentence):
                if not started:
                    started = True
                    turn["latency_ms"]["tts_first_audio"] = int((time.monotonic() - t) * 1000)
                    self.send({"type": "audio_start", "turn": turn["index"], "sample_rate": providers.TTS_SAMPLE_RATE})
                    self.send({"type": "status", "state": "speaking"})
                self.outbox.put_nowait(audio)
            self.emit_log("info", f"TTS {stage.model}: {len(sentence)} chars in {int((time.monotonic() - t) * 1000)} ms, ${cost:.6f} ({source})")
