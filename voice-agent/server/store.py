"""Saved conversations: one JSON file each, so they can be listed, resumed and deleted."""
from __future__ import annotations

import json
import re
import uuid
from datetime import datetime, timezone
from pathlib import Path

ID_RE = re.compile(r"^[0-9a-f]{12}$")
ZERO = {"llm": 0.0, "stt": 0.0, "tts": 0.0, "total": 0.0}


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


class Store:
    def __init__(self, directory: Path):
        self.dir = directory
        self.dir.mkdir(parents=True, exist_ok=True)

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
            return True
        return False

    def summaries(self) -> list[dict]:
        out = []
        for path in self.dir.glob("*.json"):
            conv = self.load(path.stem)
            if conv:
                out.append({"id": conv["id"], "title": conv.get("title", ""), "mode": conv.get("mode", ""),
                            "updated": conv.get("updated", ""), "turns": conv.get("turns", 0),
                            "total": round(conv["totals"]["total"], 6)})
        return sorted(out, key=lambda c: c["updated"], reverse=True)
