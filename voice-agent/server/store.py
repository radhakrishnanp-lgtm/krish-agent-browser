"""Saved conversations: one JSON file each, so they can be listed, resumed and deleted."""
from __future__ import annotations

import io
import json
import re
import shutil
import uuid
import wave
from datetime import datetime, timezone
from pathlib import Path

ID_RE = re.compile(r"^[0-9a-f]{12}$")
AUDIO_RE = re.compile(r"^\d{4}-(user|assistant)\.wav$")
MODE_ID_RE = re.compile(r"^custom-[0-9a-f]{8}$")
RECORDING_RATE = 24000
ZERO = {"llm": 0.0, "stt": 0.0, "tts": 0.0, "total": 0.0}


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


class Store:
    def __init__(self, directory: Path, audio_dir: Path | None = None):
        self.dir = directory
        self.dir.mkdir(parents=True, exist_ok=True)
        self.audio_dir = audio_dir or directory.parent / "audio"
        self.audio_dir.mkdir(parents=True, exist_ok=True)
        self.modes_path = directory.parent / "custom_modes.json"
        self.profile_path = directory.parent / "profile.json"
        self._audio_bytes = sum(f.stat().st_size for f in self.audio_dir.rglob("*.wav"))

    def _path(self, conv_id: str) -> Path | None:
        return self.dir / f"{conv_id}.json" if isinstance(conv_id, str) and ID_RE.match(conv_id) else None

    def create(self, mode: str) -> dict:
        now = _now()
        return {"id": uuid.uuid4().hex[:12], "title": "New conversation", "mode": mode,
                "created": now, "updated": now, "turns": 0, "totals": dict(ZERO), "messages": []}

    def load(self, conv_id: str) -> dict | None:
        path = self._path(conv_id)
        if not path or not path.exists():
            return None
        try:
            conv = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError):
            return None
        conv.setdefault("messages", [])
        conv["totals"] = {**ZERO, **(conv.get("totals") or {})}
        return conv

    def save(self, conv: dict) -> None:
        path = self._path(conv["id"])
        if not path:
            raise ValueError("invalid conversation id")
        conv["updated"] = _now()
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(conv, indent=1, ensure_ascii=False))
        tmp.replace(path)

    def delete(self, conv_id: str) -> bool:
        path = self._path(conv_id)
        if path and path.exists():
            path.unlink()
            folder = self.audio_dir / conv_id
            if folder.exists():
                self._audio_bytes -= sum(f.stat().st_size for f in folder.glob("*.wav"))
                shutil.rmtree(folder, ignore_errors=True)
            return True
        return False

    # ---------- recordings ----------

    def audio_path(self, conv_id: str, name: str) -> Path | None:
        if not self._path(conv_id) or not isinstance(name, str) or not AUDIO_RE.match(name):
            return None
        return self.audio_dir / conv_id / name

    def save_audio(self, conv_id: str, index: int, role: str, pcm: bytes, sample_rate: int) -> str | None:
        """Writes one message's audio as WAV and returns its file name."""
        name = f"{index:04d}-{role}.wav"
        path = self.audio_path(conv_id, name)
        if not path or not pcm:
            return None
        path.parent.mkdir(parents=True, exist_ok=True)
        with wave.open(str(path), "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(sample_rate)
            w.writeframes(pcm)
        self._audio_bytes += len(pcm) + 44
        return name

    def audio_mb(self) -> float:
        return max(0, self._audio_bytes) / 1_000_000

    def full_recording(self, conv: dict) -> bytes | None:
        """The whole conversation as one WAV, in order, with a short gap between messages."""
        import numpy as np
        parts = []
        gap = np.zeros(int(RECORDING_RATE * 0.45), dtype=np.int16)
        for m in conv.get("messages", []):
            path = self.audio_path(conv["id"], m.get("audio") or "")
            if not path or not path.exists():
                continue
            with wave.open(str(path), "rb") as w:
                rate = w.getframerate()
                a = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16)
            if rate != RECORDING_RATE and len(a):
                a = np.interp(np.arange(0, len(a), rate / RECORDING_RATE), np.arange(len(a)), a).astype(np.int16)
            parts += [a, gap]
        if not parts:
            return None
        buf = io.BytesIO()
        with wave.open(buf, "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(RECORDING_RATE)
            w.writeframes(np.concatenate(parts).tobytes())
        return buf.getvalue()

    # ---------- modes created in the UI ----------

    def custom_modes(self) -> dict:
        try:
            data = json.loads(self.modes_path.read_text())
            return data if isinstance(data, dict) else {}
        except (OSError, json.JSONDecodeError):
            return {}

    def save_custom_mode(self, name: str, instructions: str, greeting: str = "") -> str:
        modes = self.custom_modes()
        mode_id = "custom-" + uuid.uuid4().hex[:8]
        modes[mode_id] = {"name": name, "description": instructions[:90] + ("…" if len(instructions) > 90 else ""),
                          "instructions": instructions, "greeting": greeting, "custom": True}
        self._write_modes(modes)
        return mode_id

    def delete_custom_mode(self, mode_id: str) -> bool:
        modes = self.custom_modes()
        if not MODE_ID_RE.match(str(mode_id)) or mode_id not in modes:
            return False
        del modes[mode_id]
        self._write_modes(modes)
        return True

    # ---------- the owner's standing instructions ----------

    def profile(self) -> dict:
        try:
            data = json.loads(self.profile_path.read_text())
            return data if isinstance(data, dict) else {}
        except (OSError, json.JSONDecodeError):
            return {}

    def save_profile(self, profile: dict) -> None:
        tmp = self.profile_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(profile, indent=1, ensure_ascii=False))
        tmp.replace(self.profile_path)

    def _write_modes(self, modes: dict) -> None:
        tmp = self.modes_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(modes, indent=1, ensure_ascii=False))
        tmp.replace(self.modes_path)

    def summaries(self) -> list[dict]:
        out = []
        for path in self.dir.glob("*.json"):
            conv = self.load(path.stem)
            if conv:
                out.append({"id": conv["id"], "title": conv.get("title", ""), "mode": conv.get("mode", ""),
                            "updated": conv.get("updated", ""), "turns": conv.get("turns", 0),
                            "recorded": any(m.get("audio") for m in conv["messages"]),
                            "total": round(conv["totals"]["total"], 6)})
        return sorted(out, key=lambda c: c["updated"], reverse=True)
