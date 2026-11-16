"""Run from the voice-agent folder:  python3 -m unittest discover tests"""
import tempfile
import unittest
from pathlib import Path

import numpy as np

from server import pricing
from server.config import Config
from server.store import Store
from server.vad import TurnDetector


class PricingTest(unittest.TestCase):
    def test_llm(self):
        cost, src = pricing.llm_cost({"input_per_million": 0.035, "output_per_million": 0.37}, 120, 85)
        self.assertAlmostEqual(cost, (120 * 0.035 + 85 * 0.37) / 1e6)
        self.assertEqual(src, "pricing.yaml")

    def test_api_cost_wins(self):
        self.assertEqual(pricing.llm_cost({"input_per_million": 1}, 10, 10, api_cost=0.5), (0.5, "api"))
        self.assertEqual(pricing.stt_cost({"per_minute": 1}, 60, api_cost=0.25), (0.25, "api"))

    def test_stt_tts(self):
        self.assertAlmostEqual(pricing.stt_cost({"per_minute": 0.006}, 30)[0], 0.003)
        self.assertAlmostEqual(pricing.tts_cost({"per_1k_characters": 0.015}, 200)[0], 0.003)

    def test_missing_rate(self):
        self.assertEqual(pricing.tts_cost(None, 100), (0.0, "no-rate"))


class ConfigTest(unittest.TestCase):
    def test_every_preset_resolves_and_has_rates(self):
        cfg = Config()
        for name in cfg.presets:
            for stage in cfg.resolve(name).values():
                self.assertIsNotNone(cfg.rates(stage), f"{name}: no rate for {stage.provider}/{stage.model}")


class StoreTest(unittest.TestCase):
    def test_roundtrip_list_delete(self):
        with tempfile.TemporaryDirectory() as d:
            store = Store(Path(d))
            conv = store.create("english")
            conv["messages"].append({"role": "user", "content": "hello"})
            conv["totals"]["total"] = 0.5
            store.save(conv)
            self.assertEqual(store.load(conv["id"])["messages"][0]["content"], "hello")
            self.assertEqual([c["id"] for c in store.summaries()], [conv["id"]])
            self.assertTrue(store.delete(conv["id"]))
            self.assertIsNone(store.load(conv["id"]))

    def test_recording_saved_stitched_and_deleted(self):
        with tempfile.TemporaryDirectory() as d:
            store = Store(Path(d) / "conversations")
            conv = store.create("general")
            user = store.save_audio(conv["id"], 0, "user", np.zeros(16000, dtype=np.int16).tobytes(), 16000)
            reply = store.save_audio(conv["id"], 1, "assistant", np.zeros(24000, dtype=np.int16).tobytes(), 24000)
            conv["messages"] = [{"role": "user", "content": "a", "audio": user}, {"role": "assistant", "content": "b", "audio": reply}]
            store.save(conv)
            wav = store.full_recording(conv)
            # 1 s + 1 s of audio plus two 0.45 s gaps, at 24 kHz 16-bit, after the 44-byte header
            self.assertEqual(len(wav) - 44, int(24000 * 2.9) * 2)
            self.assertIsNone(store.audio_path(conv["id"], "../secret.wav"))
            store.delete(conv["id"])
            self.assertFalse(store.audio_path(conv["id"], user).exists())

    def test_custom_modes(self):
        with tempfile.TemporaryDirectory() as d:
            store = Store(Path(d) / "conversations")
            mode_id = store.save_custom_mode("Pirate", "Talk like a pirate at all times.")
            self.assertIn(mode_id, store.custom_modes())
            self.assertTrue(store.delete_custom_mode(mode_id))
            self.assertFalse(store.delete_custom_mode("general"))

    def test_rejects_path_tricks(self):
        with tempfile.TemporaryDirectory() as d:
            store = Store(Path(d))
            self.assertIsNone(store.load("../../etc/passwd"))
            self.assertFalse(store.delete("../x"))


class ModesTest(unittest.TestCase):
    def test_modes_have_prompts(self):
        cfg = Config()
        self.assertIn(cfg.default_mode, cfg.modes)
        for m in cfg.modes.values():
            self.assertTrue(m["system_prompt"].strip())


class VadTest(unittest.TestCase):
    def test_silence_produces_no_turn(self):
        vad = TurnDetector()
        self.assertEqual(vad.feed(np.zeros(16000 * 2, dtype=np.int16).tobytes()), [])


if __name__ == "__main__":
    unittest.main()
