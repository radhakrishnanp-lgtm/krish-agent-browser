"""Loads and validates the YAML config. Fails early with a clear message."""
from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

import yaml
from dotenv import load_dotenv

ROOT = Path(__file__).resolve().parent.parent
CONFIG_DIR = ROOT / "config"

load_dotenv(ROOT / ".env")


class ConfigError(Exception):
    pass


def _read(name: str) -> dict:
    path = CONFIG_DIR / name
    if not path.exists():
        raise ConfigError(f"Missing config file: {path}")
    try:
        return yaml.safe_load(path.read_text()) or {}
    except yaml.YAMLError as e:
        raise ConfigError(f"Invalid YAML in {path}: {e}") from e


@dataclass
class Stage:
    """One pipeline stage (llm / stt / tts) resolved to a provider + model."""
    kind: str
    provider: str
    model: str
    base_url: str
    api_key_env: str
    options: dict

    @property
    def api_key(self) -> str:
        key = os.environ.get(self.api_key_env, "").strip()
        if not key:
            raise ConfigError(
                f"{self.api_key_env} is not set. Add it to voice-agent/.env "
                f"(needed for the {self.kind.upper()} model '{self.model}')."
            )
        return key


class Config:
    def __init__(self) -> None:
        self.reload()

    def reload(self) -> None:
        self.default = _read("default.yaml")
        self.providers = _read("providers.yaml").get("providers", {})
        self.presets = _read("models.yaml").get("presets", {})
        self.pricing = _read("pricing.yaml")
        modes_doc = _read("modes.yaml")
        self.modes = modes_doc.get("modes", {})
        if not self.modes:
            raise ConfigError("config/modes.yaml defines no modes.")
        for key, m in self.modes.items():
            if not (m or {}).get("system_prompt"):
                raise ConfigError(f"Mode '{key}' in config/modes.yaml has no system_prompt.")
        self.default_mode = modes_doc.get("default_mode") or next(iter(self.modes))
        if self.default_mode not in self.modes:
            raise ConfigError(f"default_mode '{self.default_mode}' is not defined in config/modes.yaml.")
        if not self.presets:
            raise ConfigError("config/models.yaml defines no presets.")
        self.active_preset = os.environ.get("VOICE_AGENT_PRESET") or self.default.get("active_preset", "default")
        for name in self.presets:
            self.resolve(name)  # validate every preset up front
        if self.active_preset not in self.presets:
            raise ConfigError(
                f"Preset '{self.active_preset}' not found in config/models.yaml. "
                f"Available: {', '.join(self.presets)}"
            )

    def resolve(self, preset: str) -> dict[str, Stage]:
        if preset not in self.presets:
            raise ConfigError(f"Unknown preset '{preset}'. Available: {', '.join(self.presets)}")
        stages = {}
        for kind in ("llm", "stt", "tts"):
            spec = dict(self.presets[preset].get(kind) or {})
            if "provider" not in spec or "model" not in spec:
                raise ConfigError(f"Preset '{preset}' is missing {kind}.provider or {kind}.model in config/models.yaml.")
            prov = self.providers.get(spec["provider"])
            if not prov:
                raise ConfigError(
                    f"Preset '{preset}' uses provider '{spec['provider']}' for {kind}, "
                    f"which is not defined in config/providers.yaml."
                )
            stages[kind] = Stage(
                kind=kind,
                provider=spec.pop("provider"),
                model=spec.pop("model"),
                base_url=prov["base_url"].rstrip("/"),
                api_key_env=prov["api_key_env"],
                options=spec,
            )
        return stages

    def missing_keys(self, preset: str) -> list[str]:
        envs = {s.api_key_env for s in self.resolve(preset).values()}
        return sorted(e for e in envs if not os.environ.get(e, "").strip())

    def rates(self, stage: Stage) -> dict | None:
        return (self.pricing.get(stage.provider) or {}).get(stage.model)
