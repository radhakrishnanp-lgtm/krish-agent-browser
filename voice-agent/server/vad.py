"""Silero VAD (ONNX, CPU only) plus a small turn detector.

Audio in: 16 kHz mono int16 PCM. The model is run on 512-sample windows (32 ms).
"""
from __future__ import annotations

from collections import deque
from pathlib import Path

import numpy as np
import onnxruntime as ort

SAMPLE_RATE = 16000
WINDOW = 512
CONTEXT = 64
MODEL_PATH = Path(__file__).parent / "assets" / "silero_vad.onnx"

_session: ort.InferenceSession | None = None


def _get_session() -> ort.InferenceSession:
    global _session
    if _session is None:
        opts = ort.SessionOptions()
        opts.inter_op_num_threads = 1
        opts.intra_op_num_threads = 1
        _session = ort.InferenceSession(str(MODEL_PATH), sess_options=opts, providers=["CPUExecutionProvider"])
    return _session


class TurnDetector:
    """Feed PCM bytes; get back events: ("speech_start",), ("speech_confirmed",) once it is
    long enough to be real speech, then ("speech_end", pcm_bytes) or ("speech_cancel",).
    With pause_ms set, a ("speech_pause", pcm_so_far) fires after that much silence, followed
    by either ("speech_resume",) if the speaker carries on, or the normal speech_end."""

    def __init__(self, threshold=0.5, min_silence_ms=550, min_speech_ms=250, pre_roll_ms=300, max_utterance_s=30, pause_ms=0):
        self.sess = _get_session()
        self.threshold = threshold
        self.neg_threshold = max(threshold - 0.15, 0.05)
        win_ms = WINDOW * 1000 / SAMPLE_RATE
        self.silence_windows = max(1, int(min_silence_ms / win_ms))
        self.min_speech_windows = max(1, int(min_speech_ms / win_ms))
        self.max_windows = int(max_utterance_s * 1000 / win_ms)
        # Optional early "pause" event, fired before the turn officially ends.
        self.pause_windows = int(pause_ms / win_ms) if 0 < pause_ms < min_silence_ms else 0
        self.pre_roll = deque(maxlen=max(1, int(pre_roll_ms / win_ms)))
        self._buf = np.zeros(0, dtype=np.int16)
        self.reset()

    def set_threshold(self, threshold: float) -> None:
        self.threshold = threshold
        self.neg_threshold = max(threshold - 0.15, 0.05)

    def set_min_silence(self, ms: float) -> None:
        self.silence_windows = max(self.pause_windows + 1, int(ms / (WINDOW * 1000 / SAMPLE_RATE)))

    def reset(self) -> None:
        self._state = np.zeros((2, 1, 128), dtype=np.float32)
        self._context = np.zeros(CONTEXT, dtype=np.float32)
        self._speaking = False
        self._speech: list[np.ndarray] = []
        self._speech_windows = 0
        self._silence = 0
        self._paused = False
        self.pre_roll.clear()

    def _prob(self, window: np.ndarray) -> float:
        x = window.astype(np.float32) / 32768.0
        inp = np.concatenate([self._context, x])[None, :]
        out, self._state = self.sess.run(None, {"input": inp, "state": self._state, "sr": np.array(SAMPLE_RATE, dtype=np.int64)})
        self._context = x[-CONTEXT:]
        return float(out[0][0])

    def feed(self, pcm: bytes) -> list[tuple]:
        events: list[tuple] = []
        self._buf = np.concatenate([self._buf, np.frombuffer(pcm[: len(pcm) // 2 * 2], dtype=np.int16)])
        while len(self._buf) >= WINDOW:
            window, self._buf = self._buf[:WINDOW], self._buf[WINDOW:]
            p = self._prob(window)
            if not self._speaking:
                if p >= self.threshold:
                    self._speaking = True
                    self._speech = list(self.pre_roll) + [window]
                    self._speech_windows = 1
                    self._silence = 0
                    events.append(("speech_start",))
                else:
                    self.pre_roll.append(window)
                continue
            self._speech.append(window)
            if p >= self.neg_threshold:
                self._speech_windows += 1
                if self._paused:
                    self._paused = False
                    events.append(("speech_resume",))
                self._silence = 0
                if self._speech_windows == self.min_speech_windows:
                    events.append(("speech_confirmed",))
            else:
                self._silence += 1
                if (self._silence == self.pause_windows and self._speech_windows >= self.min_speech_windows
                        and len(self._speech) < self.max_windows):
                    self._paused = True
                    events.append(("speech_pause", np.concatenate(self._speech).tobytes()))
            if self._silence >= self.silence_windows or len(self._speech) >= self.max_windows:
                long_enough = self._speech_windows >= self.min_speech_windows
                audio = np.concatenate(self._speech).tobytes()
                self._speaking = False
                self._paused = False
                self._speech = []
                self.pre_roll.clear()
                events.append(("speech_end", audio) if long_enough else ("speech_cancel",))
        return events
