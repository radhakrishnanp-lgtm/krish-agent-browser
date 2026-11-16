"""FastAPI app: serves the UI and the /ws voice WebSocket.

Run:  uvicorn server.main:app --host 0.0.0.0 --port 8080   (from the voice-agent folder)
"""
from __future__ import annotations

import asyncio
import hmac
import json
import logging
import os
import re

from fastapi import FastAPI, HTTPException, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles

from .config import ROOT, Config, ConfigError
from .session import Session
from .store import Store

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)-7s %(message)s", datefmt="%H:%M:%S")
log = logging.getLogger("voice-agent")
logging.getLogger("httpx").setLevel(logging.WARNING)

cfg = Config()  # raises ConfigError with a readable message if the YAML is wrong
log.info("Active preset '%s': %s", cfg.active_preset, {k: f"{s.provider}/{s.model}" for k, s in cfg.resolve(cfg.active_preset).items()})
for _missing in cfg.missing_keys(cfg.active_preset):
    log.warning("%s is not set. Add it to voice-agent/.env; calls that need it will fail.", _missing)

ACCESS_TOKEN = os.environ.get("ACCESS_TOKEN", "").strip()
if not ACCESS_TOKEN:
    log.warning("ACCESS_TOKEN is not set: anyone who can reach this server can use your API credits.")

SESSIONS_DIR = ROOT / (cfg.default.get("logging") or {}).get("sessions_dir", "logs/sessions")
SESSION_ID = re.compile(r"^[0-9a-f-]{20,40}$")
store = Store(ROOT / (cfg.default.get("storage") or {}).get("conversations_dir", "data/conversations"))

app = FastAPI(title="Voice Agent")


def token_ok(token: str | None) -> bool:
    return not ACCESS_TOKEN or hmac.compare_digest((token or "").encode(), ACCESS_TOKEN.encode())


@app.get("/healthz")
def healthz():
    return {"ok": True, "preset": cfg.active_preset, "missing_keys": cfg.missing_keys(cfg.active_preset)}


@app.get("/api/sessions/{session_id}")
def session_log(session_id: str, token: str | None = None):
    if not token_ok(token):
        raise HTTPException(401, "Invalid access token")
    path = SESSIONS_DIR / f"{session_id}.json"
    if not SESSION_ID.match(session_id) or not path.exists():
        raise HTTPException(404, "Session log not found (it is written after the first turn)")
    return FileResponse(path, media_type="application/json", filename=path.name)


@app.websocket("/ws")
async def ws_endpoint(ws: WebSocket, token: str | None = None):
    await ws.accept()
    if not token_ok(token):
        await ws.close(code=4401, reason="Invalid access token")
        return

    outbox: asyncio.Queue = asyncio.Queue()

    async def writer():
        while True:
            item = await outbox.get()
            if isinstance(item, (bytes, bytearray)):
                await ws.send_bytes(item)
            else:
                await ws.send_text(json.dumps(item))

    writer_task = asyncio.create_task(writer())
    session = None
    try:
        try:
            cfg.reload()  # YAML edits apply to every new session without a restart
            session = Session(cfg, outbox, store)
        except ConfigError as e:
            outbox.put_nowait({"type": "log", "level": "error", "message": f"Config error: {e}"})
            outbox.put_nowait({"type": "error", "message": f"Config error: {e}"})
            await asyncio.sleep(0.2)
            await ws.close(code=1011, reason="Config error")
            return
        session.send(session.hello())
        session.emit_log("info", f"Session {session.session_id} started with preset '{session.preset}'", session.models())
        for key in cfg.missing_keys(session.preset):
            session.emit_log("error", f"{key} is not set on the server. Add it to voice-agent/.env and restart.")
        session.send_conversations()
        session.emit_cost(None)
        session.send({"type": "status", "state": "listening"})
        session.warm()

        while True:
            msg = await ws.receive()
            if msg["type"] == "websocket.disconnect":
                break
            if msg.get("bytes") is not None:
                await session.on_audio(msg["bytes"])
                continue
            try:
                data = json.loads(msg.get("text") or "{}")
            except json.JSONDecodeError:
                continue
            if not isinstance(data, dict):
                continue
            kind = data.get("type")
            if kind == "text" and str(data.get("text", "")).strip():
                session.warm()
                session.start_turn(text=str(data["text"]).strip()[:2000])
            elif kind == "new":
                await session.new_conversation(data.get("mode"), greet=bool(data.get("greet", True)))
            elif kind == "close":
                await session.cancel_turn()
                session.conv = None
                session.send_conversation()
                session.emit_cost(None)
            elif kind == "open":
                await session.open_conversation(str(data.get("id")))
            elif kind == "delete":
                await session.delete_conversation(str(data.get("id")))
            elif kind == "rename":
                session.rename_conversation(str(data.get("id")), str(data.get("title", "")))
            elif kind == "settings":
                session.set_voice_settings(data.get("voice"), data.get("speed"))
            elif kind == "set_preset":
                await session.cancel_turn()
                session.set_preset(str(data.get("name")))
            elif kind == "raw":
                session.show_raw = bool(data.get("enabled"))
                session.emit_log("info", f"Raw API responses {'on' if session.show_raw else 'off'}")
            elif kind == "stop":
                session.send({"type": "interrupt"})
                await session.cancel_turn()
    except (WebSocketDisconnect, RuntimeError):
        pass
    finally:
        if session:
            await session.close()
            if session.turns:
                log.info("Session %s ended: %d turn(s), total $%.6f, log %s",
                         session.session_id, len(session.turns), session.totals["total"], session.log_path)
        writer_task.cancel()


@app.get("/")
def index():
    return FileResponse(ROOT / "ui" / "index.html", headers={"Cache-Control": "no-cache"})


app.mount("/", StaticFiles(directory=ROOT / "ui"), name="ui")
