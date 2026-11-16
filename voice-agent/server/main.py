"""FastAPI app: serves the UI and the /ws voice WebSocket.

Run:  uvicorn server.main:app --host 0.0.0.0 --port 8080   (from the voice-agent folder)
"""
from __future__ import annotations

import asyncio
import hashlib
import hmac
import json
import logging
import os
import re

from fastapi import FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, Response
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
_storage = cfg.default.get("storage") or {}
store = Store(ROOT / _storage.get("conversations_dir", "data/conversations"), ROOT / _storage.get("audio_dir", "data/audio"))

app = FastAPI(title="Voice Agent", docs_url=None, redoc_url=None, openapi_url=None)

MAX_CONNECTIONS = int((cfg.default.get("server") or {}).get("max_connections", 20))
_connections = 0

SECURITY_HEADERS = {
    # Only this site's own scripts, styles and audio; no inline script, no framing.
    "Content-Security-Policy": "default-src 'self'; connect-src 'self' ws: wss:; media-src 'self' blob:; "
                               "img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",          # the access token travels in URLs; never leak it in a Referer
    "Permissions-Policy": "microphone=(self), camera=(), geolocation=()",
    "Cross-Origin-Resource-Policy": "same-origin",
}


@app.middleware("http")
async def security_headers(request, call_next):
    response = await call_next(request)
    for key, value in SECURITY_HEADERS.items():
        response.headers.setdefault(key, value)
    if request.url.path.startswith("/api/"):
        response.headers["Cache-Control"] = "private, no-store"
    return response


# Extra host names the UI may be served from, e.g. "voice.example.com". Needed only when a
# proxy in front of the app replaces the Host header with something else.
ALLOWED_ORIGINS = {h.strip().lower() for h in os.environ.get("ALLOWED_ORIGINS", "").split(",") if h.strip()}


def same_origin(ws: WebSocket) -> bool:
    """Blocks other websites from opening the socket from a visitor's browser
    (cross-site WebSocket hijacking). Non-browser clients send no Origin and are allowed."""
    origin = ws.headers.get("origin")
    if not origin:
        return True
    origin_host = origin.split("://", 1)[-1].lower()
    hosts = {(ws.headers.get("x-forwarded-host") or "").lower(), (ws.headers.get("host") or "").lower()} | ALLOWED_ORIGINS
    return origin_host in hosts


AUTH_COOKIE = "va_auth"
# The cookie holds a value derived from the token, so the token itself never appears in a
# URL, a log line or page-readable storage.
AUTH_VALUE = hashlib.sha256(f"voice-agent:{ACCESS_TOKEN}".encode()).hexdigest()


def _same(a: str, b: str) -> bool:
    return hmac.compare_digest(a.encode(), b.encode())


def authorized(conn) -> bool:
    """True if no token is configured, or the request carries the login cookie or a Bearer token."""
    if not ACCESS_TOKEN:
        return True
    if _same(conn.cookies.get(AUTH_COOKIE, ""), AUTH_VALUE):
        return True
    header = conn.headers.get("authorization", "")
    return header.startswith("Bearer ") and _same(header[7:], ACCESS_TOKEN)


def require_auth(request: Request) -> None:
    if not authorized(request):
        raise HTTPException(401, "Sign in with the access token first")


@app.post("/api/login")
async def login(request: Request):
    try:
        token = str((await request.json()).get("token", ""))
    except Exception:
        token = ""
    if not ACCESS_TOKEN or not _same(token, ACCESS_TOKEN):
        await asyncio.sleep(1.0)  # slows down token guessing
        raise HTTPException(401, "Wrong access token")
    response = Response(status_code=204)
    secure = request.headers.get("x-forwarded-proto", request.url.scheme) == "https"
    response.set_cookie(AUTH_COOKIE, AUTH_VALUE, max_age=60 * 60 * 24 * 90, httponly=True, samesite="strict", secure=secure, path="/")
    return response


@app.get("/healthz")
def healthz():
    return {"ok": True, "preset": cfg.active_preset, "missing_keys": cfg.missing_keys(cfg.active_preset)}


@app.get("/api/sessions/{session_id}")
def session_log(session_id: str, request: Request):
    require_auth(request)
    path = SESSIONS_DIR / f"{session_id}.json"
    if not SESSION_ID.match(session_id) or not path.exists():
        raise HTTPException(404, "Session log not found (it is written after the first turn)")
    return FileResponse(path, media_type="application/json", filename=path.name)


@app.get("/api/audio/{conv_id}/{name}")
def message_audio(conv_id: str, name: str, request: Request):
    require_auth(request)
    path = store.audio_path(conv_id, name)
    if not path or not path.exists():
        raise HTTPException(404, "Recording not found")
    return FileResponse(path, media_type="audio/wav")


@app.get("/api/conversations/{conv_id}/recording.wav")
def conversation_recording(conv_id: str, request: Request):
    require_auth(request)
    conv = store.load(conv_id)
    data = store.full_recording(conv) if conv else None
    if not data:
        raise HTTPException(404, "This conversation has no recorded audio")
    return Response(data, media_type="audio/wav", headers={"Content-Disposition": f'attachment; filename="conversation-{conv_id}.wav"'})


@app.websocket("/ws")
async def ws_endpoint(ws: WebSocket):
    global _connections
    if not same_origin(ws):
        log.warning("Refused WebSocket from origin %s (Host is %s). If this is your own site behind a proxy, "
                    "set ALLOWED_ORIGINS to its host name.", ws.headers.get("origin"), ws.headers.get("host"))
        await ws.close(code=1008)
        return
    await ws.accept()
    if not authorized(ws):
        await ws.close(code=4401, reason="Sign in first")
        return
    if _connections >= MAX_CONNECTIONS:
        await ws.close(code=1013, reason="Too many connections")
        return
    _connections += 1
    try:
        await _serve(ws)
    finally:
        _connections -= 1


async def _serve(ws: WebSocket) -> None:

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
        for key in session.missing_keys():
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
            elif kind in ("folder_create", "folder_rename", "folder_delete", "folder_order", "move"):
                session.folder_action(data)
            elif kind == "settings":
                session.apply_settings(data)
            elif kind == "set_instructions":
                session.set_instructions(data)
            elif kind == "review":
                session.start_review()
            elif kind == "save_mode":
                session.save_custom_mode(data.get("name"), data.get("instructions"), data.get("greeting"))
            elif kind == "delete_mode":
                session.delete_custom_mode(data.get("id"))
            elif kind == "set_models":
                await session.cancel_turn()
                session.set_models(data, announce=False)
            elif kind == "add_model":
                session.add_custom_model(data)
            elif kind == "delete_model":
                await session.cancel_turn()
                session.delete_custom_model(data.get("id"))
            elif kind == "test_model":
                if data.get("kind") in ("llm", "stt", "tts"):
                    session.spawn(session.test_model(data["kind"], str(data.get("id"))))
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
