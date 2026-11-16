"""One browser connection: the turn pipeline (STT -> LLM -> TTS), cost tracking, the
per-connection debug log, and the saved conversation it is attached to."""
from __future__ import annotations

import asyncio
import dataclasses
import json
import logging
import re
import time
import uuid
from datetime import datetime, timezone

from . import pricing, providers
from .config import KINDS, ROOT, Config, ConfigError, Stage, price_text
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
MAX_INSTRUCTIONS = 2000
SPOKEN_RULES = ("Your replies are spoken aloud, so keep them short and conversational: one to three "
                "sentences unless asked for more. No markdown, no lists, no emojis.")


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
        self.custom_allowed = bool((cfg.default.get("custom_models") or {}).get("enabled", True))
        self.conv: dict | None = None
        self.turns: list[dict] = []
        self.events: list[dict] = []
        self.totals = {"llm": 0.0, "stt": 0.0, "tts": 0.0, "total": 0.0}
        self.show_raw = False
        self.user_voice: str | None = None
        self.user_speed: float | None = None
        self.user_pause_ms: int | None = None
        self.record = bool((cfg.default.get("recording") or {}).get("enabled", True))
        self.record_allowed = self.record
        self.record_cap_mb = float((cfg.default.get("recording") or {}).get("max_total_mb") or 0)
        self.default_threshold = float(cfg.default.get("vad", {}).get("threshold", 0.5))
        self.review_task: asyncio.Task | None = None
        self.turn_task: asyncio.Task | None = None
        self.early: dict | None = None       # speculative STT started during a pause
        self.orphan_stt_cost = 0.0           # cost of discarded speculative STT calls
        self._bg: set[asyncio.Task] = set()
        v = cfg.default.get("vad", {})
        self.barge_in = bool(v.get("barge_in", True))
        self.default_silence_ms = int(v.get("min_silence_ms", 550))
        self.min_silence_ms = self.default_silence_ms
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
            "modes": [{"id": k, "name": m.get("name", k), "description": m.get("description", ""),
                       "custom": bool(m.get("custom")), "review": bool(m.get("review_prompt"))}
                      for k, m in self.all_modes().items()],
            "recording_allowed": self.record_allowed,
            "instructions": self.store.profile().get("instructions", ""),
            "default_mode": self.cfg.default_mode,
            "voices": list(tts.options.get("voices") or ([tts.options["voice"]] if tts.options.get("voice") else [])),
            "default_voice": tts.options.get("voice"),
            "budget_usd": self.budget,
            "barge_in": self.barge_in,
            "input_sample_rate": SAMPLE_RATE,
            "output_sample_rate": providers.TTS_SAMPLE_RATE,
            "missing_keys": self.missing_keys(),
            "catalog": self.catalog(),
            "selected": {k: s.id for k, s in self.stages.items()},
            "custom_models_allowed": self.custom_allowed,
        }

    # ---------- models ----------

    def missing_keys(self) -> list[str]:
        import os
        return sorted({s.api_key_env for s in self.stages.values() if s.key is None and not os.environ.get(s.api_key_env, "").strip()})

    def _custom_stage(self, model_id: str, spec: dict) -> Stage:
        options = {k: spec[k] for k in ("voice", "voices", "format", "language", "max_tokens") if spec.get(k)}
        return Stage(kind=spec["kind"], id=model_id, label=spec.get("label", ""), provider="custom", model=spec["model"],
                     base_url=spec["base_url"], api_key_env="", options=options, key=spec.get("api_key", ""),
                     own_rates=spec.get("rates") or {}, custom=True)

    def stage_for(self, kind: str, model_id: str) -> Stage:
        if str(model_id).startswith("custom-"):
            spec = self.store.custom_models().get(model_id)
            if not spec or spec.get("kind") != kind or not self.custom_allowed:
                raise ConfigError(f"Your own {kind} model '{model_id}' no longer exists.")
            return self._custom_stage(model_id, spec)
        return self.cfg.stage(kind, model_id)

    def catalog(self) -> dict:
        """Every model the UI can offer, with a price line. API keys are never included."""
        import os
        out = {k: [] for k in KINDS}
        for kind in KINDS:
            for model_id in self.cfg.catalog[kind]:
                s = self.cfg.stage(kind, model_id)
                out[kind].append({"id": model_id, "label": s.name, "model": s.model, "provider": s.provider,
                                  "price": price_text(kind, self.cfg.rates(s)), "custom": False,
                                  "ready": bool(os.environ.get(s.api_key_env, "").strip())})
        if self.custom_allowed:
            for model_id, spec in self.store.custom_models().items():
                if spec.get("kind") in out:
                    out[spec["kind"]].append({"id": model_id, "label": spec.get("label") or spec["model"], "model": spec["model"],
                                              "provider": spec["base_url"], "price": price_text(spec["kind"], spec.get("rates"), True),
                                              "custom": True, "ready": True})
        return out

    def set_models(self, data: dict, announce: bool = True) -> None:
        """Switch any single model: {"llm": id, "stt": id, "tts": id}; missing kinds are kept."""
        changed = []
        for kind in KINDS:
            model_id = data.get(kind)
            if not isinstance(model_id, str) or model_id == self.stages[kind].id:
                continue
            try:
                self.stages[kind] = self.stage_for(kind, model_id)
                changed.append(kind)
            except ConfigError as e:
                self.emit_log("warn", str(e))
        if "tts" in changed:
            self.user_voice = None  # voices differ between TTS models
        if changed or announce:
            self.send(self.hello())
        if changed:
            self.emit_log("info", "Models now: " + ", ".join(f"{k.upper()} {s.name}" for k, s in self.stages.items()), self.models())
            for key in self.missing_keys():
                self.emit_log("error", f"{key} is not set on the server, so that model will fail. Add it to voice-agent/.env and restart.")
            self.warm()

    def add_custom_model(self, data: dict) -> None:
        if not self.custom_allowed:
            self.emit_log("error", "Adding your own models is turned off on this server (custom_models.enabled).")
            return
        kind = data.get("kind")
        label = " ".join(str(data.get("label") or "").split())[:60]
        base_url = str(data.get("base_url") or "").strip().rstrip("/")[:300]
        model = str(data.get("model") or "").strip()[:200]
        if kind not in KINDS or not model or not re.match(r"^https?://[^\s/]+", base_url):
            self.emit_log("error", "A model needs a type, a model name and an address that starts with http:// or https://.")
            return

        def price(name: str) -> float:
            try:
                return max(0.0, float(data.get(name) or 0))
            except (TypeError, ValueError):
                return 0.0

        rates = {"llm": {"input_per_million": price("price_in"), "output_per_million": price("price_out")},
                 "stt": {"per_minute": price("price_in")}, "tts": {"per_1k_characters": price("price_in")}}[kind]
        spec = {"kind": kind, "label": label or model, "base_url": base_url, "model": model,
                "api_key": str(data.get("api_key") or "").strip()[:400], "rates": rates}
        if kind == "tts":
            voices = [v.strip()[:80] for v in str(data.get("voices") or "").split(",") if v.strip()][:40]
            if voices:
                spec["voice"], spec["voices"] = voices[0], voices
            spec["format"] = "wav" if data.get("format") == "wav" else "pcm"
        if kind == "llm":
            spec["max_tokens"] = 400
        try:
            model_id = self.store.save_custom_model(spec)
        except OSError as e:
            self.emit_log("error", f"Could not save the model: {e}")
            return
        self.emit_log("info", f"Added your {kind.upper()} model '{spec['label']}' at {base_url}")
        self.set_models({kind: model_id})
        self.spawn(self.test_model(kind, model_id))

    def delete_custom_model(self, model_id) -> None:
        spec = self.store.custom_models().get(str(model_id))
        if not spec or not self.store.delete_custom_model(model_id):
            return
        kind = spec.get("kind")
        if kind in self.stages and self.stages[kind].id == model_id:
            self.stages[kind] = self.cfg.resolve(self.preset)[kind]  # fall back to the preset's model
        self.emit_log("info", f"Removed your model '{spec.get('label')}'")
        self.send(self.hello())

    async def test_model(self, kind: str, model_id: str) -> None:
        """One small real call, so a wrong address or model name shows up immediately."""
        t = time.monotonic()
        try:
            stage = self.stage_for(kind, model_id)
            if kind == "llm":
                text = ""
                async for chunk in providers.chat_stream(dataclasses.replace(stage, options={**stage.options, "max_tokens": 20}),
                                                         [{"role": "user", "content": "Reply with the single word: ready"}]):
                    text += chunk.get("delta", "")
                detail = f'it answered "{text.strip()[:40]}"'
                if not text.strip():
                    raise ProviderError(stage, "it returned an empty reply")
            elif kind == "tts":
                n = 0
                async for audio in providers.synthesize(stage, "This is a test."):
                    n += len(audio)
                if not n:
                    raise ProviderError(stage, "it returned no audio")
                detail = f"it produced {n / 2 / providers.TTS_SAMPLE_RATE:.1f} s of audio"
            else:
                res = await providers.transcribe(stage, b"\x00" * SAMPLE_RATE * 2, SAMPLE_RATE)
                detail = f'it accepted one second of silence and returned "{res["text"][:40]}"'
        except (ProviderError, ConfigError) as e:
            self.emit_log("error", f"Model test failed. {e}")
            self.send({"type": "model_test", "id": model_id, "ok": False, "message": str(e)})
            return
        ms = int((time.monotonic() - t) * 1000)
        self.emit_log("info", f"Model test passed for {stage.name}: {detail}, in {ms} ms")
        self.send({"type": "model_test", "id": model_id, "ok": True, "message": f"{stage.name} works: {detail}, in {ms} ms."})

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
            "review": c.get("review"), "user_seconds": round(c.get("user_seconds", 0), 1),
            "instructions": c.get("instructions", ""),
            "totals": {k: round(v, 8) for k, v in c["totals"].items()}}})

    def all_modes(self) -> dict:
        """Modes from config/modes.yaml plus the ones created in the UI."""
        modes = dict(self.cfg.modes)
        for key, m in self.store.custom_modes().items():
            modes[key] = {**m, "system_prompt": f"{m.get('instructions', '').strip()}\n\n{SPOKEN_RULES}"}
        return modes

    def mode(self) -> dict:
        key = self.conv["mode"] if self.conv else self.cfg.default_mode
        modes = self.all_modes()
        return modes.get(key) or modes[self.cfg.default_mode]

    def _apply_pause(self) -> None:
        """How long a pause ends the turn: the user's setting, else the mode's, else the default."""
        ms = self.user_pause_ms or self.mode().get("min_silence_ms") or self.default_silence_ms
        self.min_silence_ms = int(ms)
        self.vad.set_min_silence(self.min_silence_ms)

    def set_instructions(self, data: dict) -> None:
        """Standing instructions for every conversation, and extra ones for the open conversation."""
        try:
            if "global" in data:
                profile = self.store.profile()
                profile["instructions"] = str(data.get("global") or "").strip()[:MAX_INSTRUCTIONS]
                self.store.save_profile(profile)
            if "conversation" in data and self.conv:
                self.conv["instructions"] = str(data.get("conversation") or "").strip()[:MAX_INSTRUCTIONS]
                self._save_conv()
        except OSError as e:
            self.emit_log("error", f"Could not save the instructions: {e}")
            return
        self.send({"type": "instructions", "global": self.store.profile().get("instructions", ""),
                   "conversation": self.conv.get("instructions", "") if self.conv else ""})
        self.emit_log("info", "Instructions saved. They apply from your next message.")

    def system_prompt(self) -> str:
        parts = [self.mode()["system_prompt"].strip()]
        mine = self.store.profile().get("instructions", "").strip()
        here = (self.conv or {}).get("instructions", "").strip()
        if mine:
            parts.append("The user's standing instructions for every conversation (follow them):\n" + mine)
        if here:
            parts.append("The user's instructions for this conversation (follow them; they win over anything above):\n" + here)
        return "\n\n".join(parts)

    def save_custom_mode(self, name, instructions, greeting) -> None:
        name = " ".join(str(name or "").split())[:40]
        instructions = str(instructions or "").strip()[:2000]
        greeting = " ".join(str(greeting or "").split())[:200]
        if not name or len(instructions) < 10:
            self.emit_log("error", "A mode needs a name and at least a sentence of instructions.")
            return
        try:
            mode_id = self.store.save_custom_mode(name, instructions, greeting)
        except OSError as e:
            self.emit_log("error", f"Could not save the mode: {e}")
            return
        self.emit_log("info", f"Created mode '{name}' ({mode_id})")
        self.send(self.hello())

    def delete_custom_mode(self, mode_id) -> None:
        if self.store.delete_custom_mode(mode_id):
            self.emit_log("info", f"Deleted mode {mode_id}")
            self.send(self.hello())

    def _save_conv(self) -> None:
        if not self.conv:
            return
        try:
            self.store.save(self.conv)
        except OSError as e:
            self.emit_log("error", f"Could not save the conversation to disk: {e}")

    async def new_conversation(self, mode: str | None, greet: bool = True) -> None:
        await self.cancel_turn()
        modes = self.all_modes()
        if mode not in modes:
            mode = self.cfg.default_mode
        self.conv = self.store.create(mode)
        self.vad.reset()
        self._apply_pause()
        self.send_conversation()
        self.emit_cost(None)
        self.emit_log("info", f"New conversation {self.conv['id']} in mode '{mode}'")
        greeting = (modes[mode].get("greeting") or "").strip()
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
        if conv["mode"] not in self.all_modes():
            self.emit_log("warn", f"Mode '{conv['mode']}' no longer exists; using '{self.cfg.default_mode}'.")
        self.conv = conv
        self.vad.reset()
        self._apply_pause()
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
        missing = self.missing_keys()
        if missing:
            self.emit_log("error", f"Missing API key(s) for this preset: {', '.join(missing)}. Add them to voice-agent/.env and restart.")
        self.warm()

    def apply_settings(self, data: dict) -> None:
        voice, speed = data.get("voice"), data.get("speed")
        self.record = self.record_allowed and data.get("record", True) is not False
        try:
            self.user_pause_ms = min(3000, max(300, int(data["pause_ms"]))) if data.get("pause_ms") else None
        except (TypeError, ValueError):
            self.user_pause_ms = None
        self._apply_pause()
        try:
            threshold = min(0.9, max(0.15, float(data["sensitivity"]))) if data.get("sensitivity") else self.default_threshold
        except (TypeError, ValueError):
            threshold = self.default_threshold
        self.vad.set_threshold(threshold)
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
        if self.review_task:
            self.review_task.cancel()
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
        if self.stages[kind].custom:
            source = source.replace("pricing.yaml", "your price")
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
        turn["_audio"] = []   # the reply's audio, kept for the recording
        user_saved = False
        try:
            if pcm:
                await self._stt(turn, pcm, stt_task)
                if not turn["user_text"]:
                    self.emit_log("warn", "No words recognised in that audio; turn skipped")
                    return
            if turn["user_text"]:
                # Save the user's words right away so nothing is lost if the reply fails.
                msg = {"role": "user", "content": turn["user_text"], "ts": turn["time"]}
                if pcm:
                    conv["user_seconds"] = conv.get("user_seconds", 0) + len(pcm) / 2 / SAMPLE_RATE
                    name = self._save_audio(conv, "user", pcm, SAMPLE_RATE)
                    if name:
                        msg["audio"] = name
                conv["messages"].append(msg)
                user_saved = True
                self.send({"type": "transcript", "role": "user", "turn": turn["index"], "text": turn["user_text"],
                           "final": True, "audio": msg.get("audio")})
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
            reply_audio = b"".join(turn.pop("_audio", []))
            reply_audio_name = None
            turn["latency_ms"]["total"] = int((time.monotonic() - t_start) * 1000)
            if turn["assistant_text"].strip():
                msg = {"role": "assistant", "content": turn["assistant_text"].strip(), "ts": now_iso(),
                       "cost": turn["turn_total"], "response_ms": turn["latency_ms"].get("response")}
                if turn["interrupted"]:
                    msg["interrupted"] = True
                reply_audio_name = self._save_audio(conv, "assistant", reply_audio, providers.TTS_SAMPLE_RATE)
                if reply_audio_name:
                    msg["audio"] = reply_audio_name
                conv["messages"].append(msg)
            if user_saved or turn["assistant_text"].strip():
                conv["turns"] = conv.get("turns", 0) + (1 if user_saved else 0)
                self._save_conv()
                self.send_conversations()
            self.send({"type": "transcript", "role": "assistant", "turn": turn["index"], "text": turn["assistant_text"],
                       "final": True, "interrupted": turn["interrupted"], "cost": turn["turn_total"],
                       "response_ms": turn["latency_ms"].get("response"), "audio": reply_audio_name,
                       "user_seconds": round(conv.get("user_seconds", 0), 1)})
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

    def _save_audio(self, conv: dict, role: str, pcm: bytes, sample_rate: int) -> str | None:
        if not self.record or not pcm:
            return None
        if self.record_cap_mb and self.store.audio_mb() >= self.record_cap_mb:
            if not getattr(self, "_cap_warned", False):
                self._cap_warned = True
                self.emit_log("warn", f"Recording paused: saved audio has reached the {self.record_cap_mb:.0f} MB limit "
                                      "(recording.max_total_mb). Delete old conversations to free space.")
            return None
        try:
            return self.store.save_audio(conv["id"], len(conv["messages"]), role, pcm, sample_rate)
        except OSError as e:
            self.emit_log("warn", f"Could not save the recording: {e}")
            return None

    # ---------- feedback report ----------

    def start_review(self) -> None:
        if self.review_task and not self.review_task.done():
            return
        self.review_task = asyncio.create_task(self._review())

    async def _review(self) -> None:
        conv, mode = self.conv, self.mode()
        prompt = (mode.get("review_prompt") or "").strip()
        if not conv or not prompt:
            self.send({"type": "review", "error": "This mode has no feedback report."})
            return
        said = [m for m in conv["messages"] if m["role"] == "user"]
        if len(said) < 2:
            self.send({"type": "review", "error": "Say at least two things first, then ask for feedback."})
            return
        self.send({"type": "review", "working": True})
        lines = [f"{'Learner' if m['role'] == 'user' else 'Partner'}: {m['content']}" for m in conv["messages"][-80:]]
        llm = self.stages["llm"]
        stage = dataclasses.replace(llm, options={**llm.options, "max_tokens": 1200})
        messages = [{"role": "system", "content": prompt}, {"role": "user", "content": "Transcript:\n" + "\n".join(lines)}]
        text, usage, t = "", None, time.monotonic()
        try:
            async for chunk in providers.chat_stream(stage, messages):
                if "delta" in chunk:
                    text += chunk["delta"]
                else:
                    usage = chunk["usage"]
        except (ProviderError, ConfigError) as e:
            self.emit_log("error", str(e))
            self.send({"type": "review", "error": str(e)})
            return
        u = usage or {"prompt_tokens": sum(len(m["content"]) for m in messages) // 4, "completion_tokens": len(text) // 4}
        cost, source = pricing.llm_cost(self.cfg.rates(llm), int(u.get("prompt_tokens") or 0), int(u.get("completion_tokens") or 0), u.get("cost"))
        for totals in (self.totals, conv["totals"]):
            totals["llm"] += cost
            totals["total"] += cost
        self.emit_log("info", f"Feedback report: {u.get('prompt_tokens')} in / {u.get('completion_tokens')} out tokens "
                              f"in {int((time.monotonic() - t) * 1000)} ms, ${cost:.6f} ({source})")
        review = {"ts": now_iso(), "messages_reviewed": len(said), "cost": round(cost, 8)}
        try:
            body = text.strip()
            body = body[body.index("{"): body.rindex("}") + 1]
            data = json.loads(body)
            if not isinstance(data, dict):
                raise ValueError
            review.update({k: data.get(k) for k in ("summary", "level", "strengths", "mistakes", "tips")})
        except ValueError:
            review["text"] = text.strip()  # the model did not return JSON: show what it wrote
        conv["review"] = review
        self._save_conv()
        self.send({"type": "review", "review": review})
        if conv is self.conv:
            self.emit_cost(None)

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
        messages = [{"role": "system", "content": self.system_prompt()}] + history
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
                    turn["_audio"].append(item)
                    self.outbox.put_nowait(item)
        finally:
            intake_task.cancel()
            for task in tasks:
                task.cancel()
