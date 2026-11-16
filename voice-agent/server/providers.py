"""Thin HTTP clients for the three cloud calls. OpenAI-compatible endpoints.

Every function raises ProviderError with a message that is safe to show in the UI.
"""
from __future__ import annotations

import io
import json
import wave
from typing import AsyncIterator

import httpx

from .config import Stage

TTS_SAMPLE_RATE = 24000  # "pcm" response format: 24 kHz mono s16le

_client: httpx.AsyncClient | None = None


def client() -> httpx.AsyncClient:
    global _client
    if _client is None:
        # Long keep-alive: a reused TLS connection saves roughly 200-300 ms per call.
        _client = httpx.AsyncClient(
            timeout=httpx.Timeout(60.0, connect=10.0),
            limits=httpx.Limits(max_keepalive_connections=10, keepalive_expiry=120.0),
        )
    return _client


_warmed: dict[str, float] = {}


async def warm(base_urls: set[str], max_age: float = 60.0) -> None:
    """Opens (or keeps open) a connection to each provider so the next real call is fast."""
    import time
    now = time.monotonic()
    for url in base_urls:
        if now - _warmed.get(url, -1e9) < max_age:
            continue
        _warmed[url] = now
        try:
            await client().head(url, timeout=5.0)
        except httpx.HTTPError:
            pass


class ProviderError(Exception):
    def __init__(self, stage: Stage, message: str, status: int | None = None, raw: str = ""):
        self.stage, self.status, self.raw = stage, status, raw
        prefix = f"{stage.kind.upper()} failed ({stage.provider} / {stage.model})"
        super().__init__(f"{prefix}: HTTP {status}: {message}" if status else f"{prefix}: {message}")


def _headers(stage: Stage) -> dict:
    return {"Authorization": f"Bearer {stage.api_key}"}


def _error_from(stage: Stage, status: int, body: str) -> ProviderError:
    msg = body.strip()[:300] or "empty response"
    try:
        err = json.loads(body).get("error")
        if isinstance(err, dict):
            msg = err.get("message") or msg
        elif isinstance(err, str):
            msg = err
    except Exception:
        pass
    hints = {401: "check the API key", 402: "the account is out of credits", 404: "check the model name", 429: "rate limited, try again shortly"}
    if status in hints:
        msg = f"{msg} ({hints[status]})"
    return ProviderError(stage, msg, status, body[:2000])


def pcm_to_wav(pcm: bytes, sample_rate: int) -> bytes:
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(sample_rate)
        w.writeframes(pcm)
    return buf.getvalue()


async def transcribe(stage: Stage, pcm: bytes, sample_rate: int, language: str | None = None) -> dict:
    """Returns {"text", "seconds", "api_cost", "raw"}."""
    seconds = len(pcm) / 2 / sample_rate
    data = {"model": stage.model}
    language = language or stage.options.get("language")
    if language:
        data["language"] = language
    try:
        r = await client().post(
            f"{stage.base_url}/audio/transcriptions",
            headers=_headers(stage),
            data=data,
            files={"file": ("speech.wav", pcm_to_wav(pcm, sample_rate), "audio/wav")},
        )
    except httpx.HTTPError as e:
        raise ProviderError(stage, f"network error: {e!r}") from e
    if r.status_code != 200:
        raise _error_from(stage, r.status_code, r.text)
    body = r.json()
    usage = body.get("usage") or {}
    return {
        "text": (body.get("text") or "").strip(),
        "seconds": float(usage.get("seconds") or seconds),
        "api_cost": usage.get("cost"),
        "raw": body,
    }


async def chat_stream(stage: Stage, messages: list[dict]) -> AsyncIterator[dict]:
    """Yields {"delta": str} chunks, then one final {"usage": {...} | None, "raw": last_chunk}."""
    body = {
        "model": stage.model,
        "messages": messages,
        "stream": True,
        "stream_options": {"include_usage": True},
        "max_tokens": stage.options.get("max_tokens", 400),
    }
    if "temperature" in stage.options:
        body["temperature"] = stage.options["temperature"]
    body.update(stage.options.get("extra") or {})
    usage, last = None, None
    try:
        async with client().stream("POST", f"{stage.base_url}/chat/completions", headers=_headers(stage), json=body) as r:
            if r.status_code != 200:
                raise _error_from(stage, r.status_code, (await r.aread()).decode("utf-8", "replace"))
            async for line in r.aiter_lines():
                if not line.startswith("data:"):
                    continue
                payload = line[5:].strip()
                if payload == "[DONE]":
                    break
                try:
                    chunk = json.loads(payload)
                except json.JSONDecodeError:
                    continue
                if chunk.get("error"):
                    err = chunk["error"]
                    raise ProviderError(stage, err.get("message", str(err)) if isinstance(err, dict) else str(err))
                last = chunk
                if chunk.get("usage"):
                    usage = chunk["usage"]
                for choice in chunk.get("choices") or []:
                    text = (choice.get("delta") or {}).get("content")
                    if text:
                        yield {"delta": text}
    except httpx.HTTPError as e:
        raise ProviderError(stage, f"network error: {e!r}") from e
    yield {"usage": usage, "raw": last}


async def synthesize(stage: Stage, text: str, voice: str | None = None, speed: float | None = None) -> AsyncIterator[bytes]:
    """Yields raw PCM (24 kHz mono s16le) as it arrives."""
    body = {"model": stage.model, "input": text, "response_format": "pcm"}
    voice = voice or stage.options.get("voice")
    speed = speed or stage.options.get("speed")
    if voice:
        body["voice"] = voice
    if speed and abs(float(speed) - 1.0) > 0.01:
        body["speed"] = float(speed)
    try:
        async with client().stream("POST", f"{stage.base_url}/audio/speech", headers=_headers(stage), json=body) as r:
            if r.status_code != 200:
                raise _error_from(stage, r.status_code, (await r.aread()).decode("utf-8", "replace"))
            ctype = r.headers.get("content-type", "")
            if "pcm" not in ctype and "octet-stream" not in ctype and "l16" not in ctype.lower():
                raise ProviderError(stage, f"expected PCM audio but got '{ctype}'. This TTS model may not support response_format=pcm.")
            carry = b""
            async for chunk in r.aiter_bytes(8192):
                chunk = carry + chunk
                cut = len(chunk) // 2 * 2  # keep 16-bit samples whole
                carry = chunk[cut:]
                if cut:
                    yield chunk[:cut]
    except httpx.HTTPError as e:
        raise ProviderError(stage, f"network error: {e!r}") from e
