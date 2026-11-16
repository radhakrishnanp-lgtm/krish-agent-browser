"""One browser connection: the turn pipeline (STT -> LLM -> TTS), cost tracking, the
per-connection debug log, and the saved conversation it is attached to."""
from __future__ import annotations

import asyncio
import json
import logging
import re
import time
import uuid
from datetime import datetime, timezone

from . import pricing, providers
from .config import ROOT, Config, ConfigError
from .providers import ProviderError
from .store import Store
from .vad import SAMPLE_RATE, TurnDetector

log = logging.getLogger("voice-agent")

SENTENCE_END = re.compile(r"(?<=[.!?。！？])[\"')\]]*\s+|\n+")
CLAUSE_END = re.compile(r"(?<=[,;:])\s+|\s+[–—-]\s+")
MAX_SAVED_EVENTS = 2000
MIN_TTS_CHARS = 12          # shorter sentences ("Hi!") are merged with the next one
FIRST_CLAUSE_CHARS = 60     # a long first sentence is split at a comma so audio starts sooner
TTS_PARALLEL = 3            # sentences synthesised at once (played back in order)
TITLE_CHARS = 48


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds")


class Session:
    def __init__(self, cfg: Config, outbox: asyncio.Queue, store: Store):
        self.cfg = cfg
        self.outbox = outbox
        self.store = store
        self.start_time = now_iso()
        self.session_id = datetime.now().strftime("%Y-%m-%d-%H-%M-%S") + "-" + uuid.uuid4().hex[:8]
        self.preset = cfg.active_preset
        self.stages = cfg.resolve(self.preset)
        self.conv: dict | None = None
        self.turns: list[dict] = []
        self.events: list[dict] = []
        self.totals = {"llm": 0.0, "stt": 0.0, "tts": 0.0, "total": 0.0}
        self.show_raw = False
        self.user_voice: str | None = None
        self.user_speed: float | None = None
        self.turn_task: asyncio.Task | None = None
        self.early: dict | None = None       # speculative STT started during a pause
        self.orphan_stt_cost = 0.0           # cost of discarded speculative STT calls
        self._bg: set[asyncio.Task] = set()
        v = cfg.default.get("vad", {})
        self.barge_in = bool(v.get("barge_in", True))
        self.min_silence_ms = int(v.get("min_silence_ms", 550))
        self.vad = TurnDetector(
            threshold=v.get("threshold", 0.5),
            min_silence_ms=self.min_silence_ms,
            min_speech_ms=v.get("min_speech_ms", 250),
            pre_roll_ms=v.get("pre_roll_ms", 300),
            max_utterance_s=v.get("max_utterance_s", 30),
            pause_ms=v.get("early_stt_ms", 250),
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

    def spawn(self, coro) -> asyncio.Task:
        task = asyncio.create_task(coro)
        self._bg.add(task)
        task.add_done_callback(self._bg.discard)
        return task

    def warm(self) -> None:
        self.spawn(providers.warm({s.base_url for s in self.stages.values()}))

    def hello(self) -> dict:
        tts = self.stages["tts"]
        return {
            "type": "hello",
            "session_id": self.session_id,
            "preset": self.preset,
            "presets": list(self.cfg.presets),
            "models": self.models(),
            "modes": [{"id": k, "name": m.get("name", k), "description": m.get("description", "")} for k, m in self.cfg.modes.items()],
            "default_mode": self.cfg.default_mode,
            "voices": list(tts.options.get("voices") or ([tts.options["voice"]] if tts.options.get("voice") else [])),
            "default_voice": tts.options.get("voice"),
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
            "conversation": {k: round(v, 8) for k, v in self.conv["totals"].items()} if self.conv else None,
            "budget_usd": self.budget,
        })

    # ---------- per-connection debug log ----------

    def save_log(self) -> None:
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

    # ---------- conversations ----------

    def send_conversations(self) -> None:
        self.send({"type": "conversations", "items": self.store.summaries()})

    def send_conversation(self) -> None:
        c = self.conv
        self.send({"type": "conversation", "conversation": c and {
            "id": c["id"], "title": c["title"], "mode": c["mode"], "messages": c["messages"],
            "totals": {k: round(v, 8) for k, v in c["totals"].items()}}})

    def mode(self) -> dict:
        key = self.conv["mode"] if self.conv else self.cfg.default_mode
        return self.cfg.modes.get(key) or self.cfg.modes[self.cfg.default_mode]

    def _save_conv(self) -> None:
        if not self.conv:
            return
        try:
            self.store.save(self.conv)
        except OSError as e:
            self.emit_log("error", f"Could not save the conversation to disk: {e}")

    async def new_conversation(self, mode: str | None, greet: bool = True) -> None:
        await self.cancel_turn()
        if mode not in self.cfg.modes:
            mode = self.cfg.default_mode
        self.conv = self.store.create(mode)
        self.vad.reset()
        self.send_conversation()
        self.emit_cost(None)
        self.emit_log("info", f"New conversation {self.conv['id']} in mode '{mode}'")
        greeting = (self.cfg.modes[mode].get("greeting") or "").strip()
        if greet and greeting:
            self.start_turn(greeting=greeting)

    async def open_conversation(self, conv_id: str) -> None:
        await self.cancel_turn()
        conv = self.store.load(conv_id)
        if not conv:
            self.conv = None
            self.send_conversation()
            self.send_conversations()
            self.emit_log("warn", "That conversation no longer exists.")
            return
        if conv["mode"] not in self.cfg.modes:
            self.emit_log("warn", f"Mode '{conv['mode']}' is no longer in config/modes.yaml; using '{self.cfg.default_mode}'.")
        self.conv = conv
        self.vad.reset()
        self.send_conversation()
        self.emit_cost(None)
        self.emit_log("info", f"Resumed conversation {conv['id']} \"{conv['title']}\" ({len(conv['messages'])} messages)")

    async def delete_conversation(self, conv_id: str) -> None:
        if self.conv and self.conv["id"] == conv_id:
            await self.cancel_turn()
            self.conv = None
            self.send_conversation()
        if self.store.delete(conv_id):
            self.emit_log("info", f"Deleted conversation {conv_id}")
        self.send_conversations()

    def rename_conversation(self, conv_id: str, title: str) -> None:
        title = " ".join(title.split())[:80]
        if not title:
            return
        conv = self.conv if self.conv and self.conv["id"] == conv_id else self.store.load(conv_id)
        if not conv:
            return
        conv["title"] = title
        conv["titled"] = True
        try:
            self.store.save(conv)
        except OSError as e:
            self.emit_log("error", f"Could not save the conversation: {e}")
        if conv is self.conv:
            self.send({"type": "conversation_meta", "id": conv["id"], "title": title})
        self.send_conversations()

    # ---------- control messages ----------

    def set_preset(self, name: str) -> None:
        try:
            self.cfg.reload()  # pick up YAML edits without a restart
            self.stages = self.cfg.resolve(name)
        except ConfigError as e:
            self.emit_log("error", f"Could not switch preset: {e}")
            return
        self.preset = name
        self.user_voice = None  # voices differ between TTS models
        self.send(self.hello())
        self.emit_log("info", f"Preset switched to '{name}'", self.models())
        missing = self.cfg.missing_keys(name)
        if missing:
            self.emit_log("error", f"Missing API key(s) for this preset: {', '.join(missing)}. Add them to voice-agent/.env and restart.")
        self.warm()

    def set_voice_settings(self, voice, speed) -> None:
        allowed = self.stages["tts"].options.get("voices") or []
        self.user_voice = voice if isinstance(voice, str) and voice in allowed else None
        try:
            self.user_speed = min(1.3, max(0.7, float(speed))) if speed else None
        except (TypeError, ValueError):
            self.user_speed = None

    async def cancel_turn(self) -> None:
        task = self.turn_task
        if task and not task.done():
            task.cancel()
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass

    async def close(self) -> None:
        await self.cancel_turn()
        self._drop_early()
        for task in list(self._bg):
            task.cancel()
        if self.turns:
            self.save_log()

    # ---------- audio in ----------

    def _busy(self) -> bool:
        return self.turn_task is not None and not self.turn_task.done()

    def _drop_early(self) -> None:
        """Discard a speculative transcription (the speaker carried on). Its cost still counts."""
        early, self.early = self.early, None
        if not early:
            return

        def account(task: asyncio.Task) -> None:
            if not task.cancelled() and not task.exception():
                res = task.result()
                cost, _ = pricing.stt_cost(self.cfg.rates(self.stages["stt"]), res["seconds"], res["api_cost"])
                self.orphan_stt_cost += cost

        early["task"].add_done_callback(account)

    async def on_audio(self, pcm: bytes) -> None:
        for ev in self.vad.feed(pcm):
            kind = ev[0]
            if kind == "speech_start":
                self.send({"type": "status", "state": "user_speaking"})
                self.warm()
            elif kind == "speech_confirmed":
                if self.barge_in:
                    self.send({"type": "interrupt"})
                    if self._busy():
                        self.emit_log("info", "Barge-in: user spoke, stopping the assistant")
                        await self.cancel_turn()
            elif kind == "speech_pause":
                if not (self._busy() and not self.barge_in):
                    self._drop_early()
                    self.early = {"task": self.spawn(self._transcribe(ev[1])), "t": time.monotonic()}
            elif kind == "speech_resume":
                self._drop_early()
            elif kind == "speech_cancel":
                self._drop_early()
                self.send({"type": "status", "state": "thinking" if self._busy() else "listening"})
            elif kind == "speech_end":
                if self._busy() and not self.barge_in:
                    self._drop_early()
                    self.emit_log("warn", "Ignored speech while the assistant was replying (barge_in is off)")
                    continue
                early, self.early = self.early, None
                self.start_turn(pcm=ev[1], stt_task=early["task"] if early else None)

    def _transcribe(self, pcm: bytes):
        return providers.transcribe(self.stages["stt"], pcm, SAMPLE_RATE, language=self.mode().get("stt_language"))

    def start_turn(self, pcm: bytes | None = None, text: str | None = None, greeting: str | None = None,
                   stt_task: asyncio.Task | None = None) -> None:
        previous = self.turn_task
        if previous and not previous.done():
            self.send({"type": "interrupt"})
        t_start = time.monotonic()

        async def runner():
            if previous and not previous.done():
                previous.cancel()
                try:
                    await previous
                except (asyncio.CancelledError, Exception):
                    pass
            await self.run_turn(pcm, text, greeting, stt_task, t_start)

        self.turn_task = asyncio.create_task(runner())

    # ---------- the turn pipeline ----------

    def _add_cost(self, turn: dict, kind: str, cost: float, source: str) -> None:
        turn[f"{kind}_cost"] = round(turn[f"{kind}_cost"] + cost, 8)
        turn["cost_sources"][kind] = source
        turn["turn_total"] = round(turn["llm_cost"] + turn["stt_cost"] + turn["tts_cost"], 8)
        self.totals[kind] += cost
        self.totals["total"] += cost
        if self.conv:
            self.conv["totals"][kind] += cost
            self.conv["totals"]["total"] += cost
        if source == "no-rate":
            stage = self.stages[kind]
            self.emit_log("warn", f"No rate for {stage.provider} / {stage.model} in config/pricing.yaml; its cost is counted as $0")
        self.emit_cost(turn)

    async def run_turn(self, pcm, text, greeting, stt_task, t_start) -> None:
        if self.budget and self.totals["total"] >= self.budget:
            self.emit_log("error", f"Session budget of ${self.budget:.2f} reached (spent ${self.totals['total']:.4f}). "
                                   "Raise cost.session_budget_usd in config/default.yaml or reload the page for a new session.")
            self.send({"type": "status", "state": "listening"})
            return
        if not self.conv:  # speaking or typing with no conversation open starts one
            self.conv = self.store.create(self.cfg.default_mode)
            self.send_conversation()
        conv = self.conv

        turn = {
            "index": len(self.turns) + 1, "time": now_iso(), "conversation_id": conv["id"], "mode": conv["mode"],
            "input": "greeting" if greeting else "voice" if pcm else "text",
            "user_text": text or "", "assistant_text": "",
            "llm_input_tokens": 0, "llm_output_tokens": 0, "llm_cost": 0.0,
            "stt_seconds": 0.0, "stt_cost": 0.0,
            "tts_characters": 0, "tts_cost": 0.0,
            "turn_total": 0.0, "cost_sources": {}, "latency_ms": {}, "interrupted": False, "errors": [],
        }
        self.turns.append(turn)
        turn["_t0"] = t_start
        user_saved = False
        try:
            if pcm:
                await self._stt(turn, pcm, stt_task)
                if not turn["user_text"]:
                    self.emit_log("warn", "No words recognised in that audio; turn skipped")
                    return
            if turn["user_text"]:
                self.send({"type": "transcript", "role": "user", "turn": turn["index"], "text": turn["user_text"], "final": True})
                # Save the user's words right away so nothing is lost if the reply fails.
                conv["messages"].append({"role": "user", "content": turn["user_text"], "ts": turn["time"]})
                user_saved = True
                if not conv.get("titled") and conv["title"] == "New conversation":
                    t = " ".join(turn["user_text"].split())
                    conv["title"] = t[:TITLE_CHARS].rstrip() + ("…" if len(t) > TITLE_CHARS else "")
                    self.send({"type": "conversation_meta", "id": conv["id"], "title": conv["title"]})
            await self._reply(turn, greeting)
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
            turn.pop("_t0", None)
            turn["latency_ms"]["total"] = int((time.monotonic() - t_start) * 1000)
            if turn["assistant_text"].strip():
                msg = {"role": "assistant", "content": turn["assistant_text"].strip(), "ts": now_iso(),
                       "cost": turn["turn_total"], "response_ms": turn["latency_ms"].get("response")}
                if turn["interrupted"]:
                    msg["interrupted"] = True
                conv["messages"].append(msg)
            if user_saved or turn["assistant_text"].strip():
                conv["turns"] = conv.get("turns", 0) + (1 if user_saved else 0)
                self._save_conv()
                self.send_conversations()
            self.send({"type": "transcript", "role": "assistant", "turn": turn["index"], "text": turn["assistant_text"],
                       "final": True, "interrupted": turn["interrupted"], "cost": turn["turn_total"],
                       "response_ms": turn["latency_ms"].get("response")})
            self.send({"type": "audio_end", "turn": turn["index"]})
            lat = turn["latency_ms"]
            self.emit_log("info",
                f"Turn {turn['index']} done: llm={self.stages['llm'].model} in={turn['llm_input_tokens']} out={turn['llm_output_tokens']} "
                f"| llm ${turn['llm_cost']:.6f} + stt ${turn['stt_cost']:.6f} + tts ${turn['tts_cost']:.6f} = ${turn['turn_total']:.6f} "
                f"| session ${self.totals['total']:.6f}"
                + (f" | first audio after {lat['response']} ms" if "response" in lat else "")
                + (" (interrupted)" if turn["interrupted"] else ""),
                {"turn": turn})
            self.emit_cost(turn)
            self.send({"type": "status", "state": "listening"})
            self.save_log()

    async def _stt(self, turn: dict, pcm: bytes, stt_task: asyncio.Task | None) -> None:
        stage = self.stages["stt"]
        self.send({"type": "status", "state": "transcribing"})
        t = time.monotonic()
        early = stt_task is not None
        try:
            res = await (stt_task or self._transcribe(pcm))
        except asyncio.CancelledError:
            if stt_task:
                stt_task.cancel()
            raise
        turn["latency_ms"]["stt_wait"] = int((time.monotonic() - t) * 1000)
        turn["user_text"] = res["text"]
        turn["stt_seconds"] = round(res["seconds"], 3)
        self.emit_raw("stt", res["raw"])
        cost, source = pricing.stt_cost(self.cfg.rates(stage), res["seconds"], res["api_cost"])
        extra, self.orphan_stt_cost = self.orphan_stt_cost, 0.0
        self.emit_log("info", f"STT {stage.model}: {res['seconds']:.1f}s audio, waited {turn['latency_ms']['stt_wait']} ms"
                              f"{' (started early during the pause)' if early else ''}, ${cost:.6f} ({source})"
                              + (f" + ${extra:.6f} for discarded early transcriptions" if extra else ""))
        self._add_cost(turn, "stt", cost + extra, source)

    def _first_audio(self, turn: dict) -> None:
        if "first_audio" in turn["latency_ms"]:
            return
        ms = int((time.monotonic() - turn["_t0"]) * 1000)
        turn["latency_ms"]["first_audio"] = ms
        if turn["input"] != "greeting":
            # For voice, add the silence the VAD waited before deciding the user had finished,
            # so the number is "you stopped talking -> you hear the reply".
            turn["latency_ms"]["response"] = ms + (self.min_silence_ms if turn["input"] == "voice" else 0)
        self.send({"type": "audio_start", "turn": turn["index"], "sample_rate": providers.TTS_SAMPLE_RATE})
        self.send({"type": "status", "state": "speaking"})

    async def _reply(self, turn: dict, greeting: str | None) -> None:
        sentences: asyncio.Queue = asyncio.Queue()
        tts_task = asyncio.create_task(self._tts_pipeline(turn, sentences))
        completed = False
        try:
            if greeting:
                turn["assistant_text"] = greeting
                self.send({"type": "transcript", "role": "assistant", "turn": turn["index"], "text": greeting, "final": False})
                sentences.put_nowait(greeting)
            else:
                await self._llm(turn, sentences)
            completed = True
        finally:
            if not completed:
                tts_task.cancel()
        sentences.put_nowait(None)
        try:
            await tts_task
        except asyncio.CancelledError:
            tts_task.cancel()
            raise

    async def _llm(self, turn: dict, sentences: asyncio.Queue) -> None:
        llm = self.stages["llm"]
        max_msgs = int((self.cfg.default.get("assistant") or {}).get("max_history_turns", 20)) * 2
        history = [{"role": m["role"], "content": m["content"]} for m in self.conv["messages"][-(max_msgs + 1):]]
        messages = [{"role": "system", "content": self.mode()["system_prompt"].strip()}] + history
        self.send({"type": "status", "state": "thinking"})
        t = time.monotonic()
        usage, pending, first, sent_any = None, "", True, False
        try:
            async for chunk in providers.chat_stream(llm, messages):
                if "delta" not in chunk:
                    usage = chunk["usage"]
                    self.emit_raw("llm", chunk["raw"])
                    continue
                if first:
                    turn["latency_ms"]["llm_first_token"] = int((time.monotonic() - t) * 1000)
                    first = False
                turn["assistant_text"] += chunk["delta"]
                pending += chunk["delta"]
                self.send({"type": "transcript", "role": "assistant", "turn": turn["index"], "text": turn["assistant_text"], "final": False})
                # Hand complete sentences to TTS as they arrive.
                cut = 0
                for m in SENTENCE_END.finditer(pending):
                    if m.start() - cut >= MIN_TTS_CHARS:
                        sentences.put_nowait(pending[cut:m.start()].strip())
                        sent_any = True
                        cut = m.end()
                pending = pending[cut:]
                if not sent_any and len(pending) >= FIRST_CLAUSE_CHARS:
                    m = None
                    for m in CLAUSE_END.finditer(pending):
                        pass  # last clause break so far
                    if m and m.start() >= MIN_TTS_CHARS * 2:
                        sentences.put_nowait(pending[:m.start()].strip())
                        sent_any = True
                        pending = pending[m.end():]
            if pending.strip():
                sentences.put_nowait(pending.strip())
        finally:
            turn["latency_ms"]["llm"] = int((time.monotonic() - t) * 1000)
            # Bill even if interrupted mid-stream (tokens were generated), but not if the
            # call failed before producing anything.
            if usage is not None or turn["assistant_text"]:
                self._bill_llm(turn, llm, messages, usage)
        if not turn["assistant_text"].strip():
            raise ProviderError(llm, "the model returned an empty reply")

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
        self.emit_log("info", f"LLM {llm.model}: {turn['llm_input_tokens']} in / {turn['llm_output_tokens']} out tokens, "
                              f"first token {turn['latency_ms'].get('llm_first_token', '?')} ms, done {turn['latency_ms']['llm']} ms, ${cost:.6f} ({source})")
        self._add_cost(turn, "llm", cost, source)

    async def _tts_pipeline(self, turn: dict, sentences: asyncio.Queue) -> None:
        """Synthesises up to TTS_PARALLEL sentences at once and forwards the audio in order."""
        stage = self.stages["tts"]
        voice = self.user_voice
        speed = self.user_speed or self.mode().get("tts_speed")
        limit = asyncio.Semaphore(TTS_PARALLEL)
        jobs: asyncio.Queue = asyncio.Queue()
        tasks: list[asyncio.Task] = []

        async def synth(text: str, out: asyncio.Queue) -> None:
            try:
                async with limit:
                    t = time.monotonic()
                    # Billed for the characters submitted, so count the cost up front.
                    cost, source = pricing.tts_cost(self.cfg.rates(stage), len(text))
                    turn["tts_characters"] += len(text)
                    self._add_cost(turn, "tts", cost, source)
                    first = None
                    async for audio in providers.synthesize(stage, text, voice=voice, speed=speed):
                        first = first or int((time.monotonic() - t) * 1000)
                        out.put_nowait(audio)
                    self.emit_log("info", f"TTS {stage.model}: {len(text)} chars, first audio {first} ms, "
                                          f"done {int((time.monotonic() - t) * 1000)} ms, ${cost:.6f} ({source})")
                out.put_nowait(None)
            except asyncio.CancelledError:
                raise
            except Exception as e:
                out.put_nowait(e)

        async def intake() -> None:
            while True:
                text = await sentences.get()
                if text is None:
                    jobs.put_nowait(None)
                    return
                out: asyncio.Queue = asyncio.Queue()
                tasks.append(asyncio.create_task(synth(text, out)))
                jobs.put_nowait(out)

        intake_task = asyncio.create_task(intake())
        try:
            while True:
                out = await jobs.get()
                if out is None:
                    return
                while True:
                    item = await out.get()
                    if item is None:
                        break
                    if isinstance(item, Exception):
                        raise item
                    self._first_audio(turn)
                    self.outbox.put_nowait(item)
        finally:
            intake_task.cancel()
            for task in tasks:
                task.cancel()
