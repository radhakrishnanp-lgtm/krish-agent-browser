"""Thin HTTP clients for the three cloud calls. OpenAI-compatible endpoints.

Every function raises ProviderError with a message that is safe to show in the UI.
"""
from __future__ import annotations

import io
import json
import re
import wave
from typing import AsyncIterator

import httpx
import numpy as np

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
        prefix = f"{stage.kind.upper()} failed ({stage.name})"
        super().__init__(f"{prefix}: HTTP {status}: {message}" if status else f"{prefix}: {message}")


def _headers(stage: Stage) -> dict:
    key = stage.api_key
    return {"Authorization": f"Bearer {key}"} if key else {}  # a local server may need no key


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


def _network_message(stage: Stage, e: Exception) -> str:
    if isinstance(e, httpx.ConnectError):
        return (f"could not connect to {stage.base_url}. Check the address, that the model server is running, "
                "and that this server can reach it")
    if isinstance(e, httpx.TimeoutException):
        return f"{stage.base_url} did not answer in time"
    return f"network error: {e!r}"


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
        raise ProviderError(stage, _network_message(stage, e)) from e
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
        raise ProviderError(stage, _network_message(stage, e)) from e
    yield {"usage": usage, "raw": last}


class _Resampler:
    """Streaming linear resampler to TTS_SAMPLE_RATE, for endpoints that answer at another rate."""

    def __init__(self, rate: int):
        self.step = rate / TTS_SAMPLE_RATE
        self.pos = 0.0
        self.last = np.zeros(1, dtype=np.float32)

    def feed(self, pcm: bytes) -> bytes:
        x = np.concatenate([self.last, np.frombuffer(pcm, dtype=np.int16).astype(np.float32)])
        idx = np.arange(self.pos, len(x) - 1, self.step)
        if not len(idx):
            self.last = x
            return b""
        out = np.interp(idx, np.arange(len(x)), x).astype(np.int16)
        self.pos = idx[-1] + self.step - (len(x) - 1)
        self.last = x[-1:]
        return out.tobytes()


def _wav_to_pcm(data: bytes) -> bytes:
    with wave.open(io.BytesIO(data), "rb") as w:
        if w.getsampwidth() != 2:
            raise ValueError(f"{w.getsampwidth() * 8}-bit audio; only 16-bit WAV is supported")
        rate, channels = w.getframerate(), w.getnchannels()
        a = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16)
    if channels > 1:
        a = a.reshape(-1, channels).mean(axis=1).astype(np.int16)
    return a.tobytes() if rate == TTS_SAMPLE_RATE else _Resampler(rate).feed(a.tobytes())


async def synthesize(stage: Stage, text: str, voice: str | None = None, speed: float | None = None) -> AsyncIterator[bytes]:
    """Yields raw PCM (24 kHz mono s16le) as it arrives, whatever rate the endpoint answers in.

    With the tts option `format: wav` the whole WAV file is fetched and converted, for
    servers that cannot stream raw PCM.
    """
    fmt = stage.options.get("format", "pcm")
    body = {"model": stage.model, "input": text, "response_format": fmt}
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
            ctype = r.headers.get("content-type", "").lower()
            if fmt == "wav" or "wav" in ctype:
                try:
                    pcm = _wav_to_pcm(await r.aread())
                except (wave.Error, ValueError, EOFError) as e:
                    raise ProviderError(stage, f"could not read the WAV audio it returned: {e}") from e
                for i in range(0, len(pcm), 8192):
                    yield pcm[i:i + 8192]
                return
            if "json" in ctype or "text" in ctype or "mpeg" in ctype or "mp3" in ctype or "ogg" in ctype:
                raise ProviderError(stage, f"expected raw PCM audio but got '{ctype}'. Set this model's format to wav, "
                                           "or pick a model that supports response_format=pcm.")
            m = re.search(r"rate=(\d+)", ctype)
            rate = int(m.group(1)) if m else int(stage.options.get("sample_rate", TTS_SAMPLE_RATE))
            resampler = _Resampler(rate) if rate != TTS_SAMPLE_RATE else None
            carry = b""
            async for chunk in r.aiter_bytes(8192):
                chunk = carry + chunk
                cut = len(chunk) // 2 * 2  # keep 16-bit samples whole
                carry = chunk[cut:]
                if cut:
                    out = resampler.feed(chunk[:cut]) if resampler else chunk[:cut]
                    if out:
                        yield out
    except httpx.HTTPError as e:
        raise ProviderError(stage, _network_message(stage, e)) from e
