"""Loads and validates the YAML config. Fails early with a clear message."""
from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

import yaml
from dotenv import load_dotenv

ROOT = Path(__file__).resolve().parent.parent
CONFIG_DIR = ROOT / "config"
KINDS = ("llm", "stt", "tts")

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
    """One pipeline stage (llm / stt / tts) resolved to an endpoint + model."""
    kind: str
    provider: str
    model: str
    base_url: str
    api_key_env: str
    options: dict
    id: str = ""
    label: str = ""
    key: str | None = None        # set for models added in the UI: the key itself ("" = none needed)
    own_rates: dict | None = None  # set for models added in the UI: their price, not pricing.yaml's
    custom: bool = field(default=False)

    @property
    def api_key(self) -> str:
        """The key to send, or "" when the endpoint needs none (a local server)."""
        if self.key is not None:
            return self.key
        key = os.environ.get(self.api_key_env, "").strip()
        if not key:
            raise ConfigError(
                f"{self.api_key_env} is not set. Add it to voice-agent/.env "
                f"(needed for the {self.kind.upper()} model '{self.model}')."
            )
        return key

    @property
    def name(self) -> str:
        return self.label or self.model


def price_text(kind: str, rates: dict | None, custom: bool = False) -> str:
    """A short price for the UI, e.g. "$0.14 in / $0.42 out per 1M tokens"."""
    def money(v: float) -> str:
        return "$" + (f"{v:.5f}".rstrip("0").rstrip(".") if v else "0")
    if not rates:
        return "free (your own endpoint)" if custom else "no price set"
    if kind == "llm" and "input_per_million" in rates:
        if not rates["input_per_million"] and not rates.get("output_per_million"):
            return "free (your own endpoint)"
        return f"{money(rates['input_per_million'])} in / {money(rates.get('output_per_million', 0))} out per 1M tokens"
    if kind == "stt" and "per_minute" in rates:
        return f"{money(rates['per_minute'])} per minute" if rates["per_minute"] else "free (your own endpoint)"
    if kind == "tts" and "per_1k_characters" in rates:
        return f"{money(rates['per_1k_characters'])} per 1,000 characters" if rates["per_1k_characters"] else "free (your own endpoint)"
    return "no price set"


class Config:
    def __init__(self) -> None:
        self.reload()

    def reload(self) -> None:
        self.default = _read("default.yaml")
        self.providers = _read("providers.yaml").get("providers", {})
        models = _read("models.yaml")
        self.catalog = models.get("catalog") or {}
        self.presets = models.get("presets", {})
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
        for kind in KINDS:
            if not self.catalog.get(kind):
                raise ConfigError(f"config/models.yaml has no catalog.{kind} entries.")
            for model_id in self.catalog[kind]:
                self.stage(kind, model_id)  # validate every catalog entry up front
        if not self.presets:
            raise ConfigError("config/models.yaml defines no presets.")
        for name in self.presets:
            self.resolve(name)
        self.active_preset = os.environ.get("VOICE_AGENT_PRESET") or self.default.get("active_preset", "default")
        if self.active_preset not in self.presets:
            raise ConfigError(
                f"Preset '{self.active_preset}' not found in config/models.yaml. "
                f"Available: {', '.join(self.presets)}"
            )

    def stage(self, kind: str, model_id: str) -> Stage:
        """Builds a Stage from a catalog entry."""
        spec = dict((self.catalog.get(kind) or {}).get(model_id) or {})
        if not spec:
            raise ConfigError(f"No {kind} model '{model_id}' in the catalog of config/models.yaml.")
        if "provider" not in spec or "model" not in spec:
            raise ConfigError(f"catalog.{kind}.{model_id} in config/models.yaml needs both provider and model.")
        prov = self.providers.get(spec["provider"])
        if not prov:
            raise ConfigError(f"catalog.{kind}.{model_id} uses provider '{spec['provider']}', "
                              f"which is not defined in config/providers.yaml.")
        key_env = prov.get("api_key_env") or ""
        return Stage(kind=kind, id=model_id, label=spec.pop("label", ""), provider=spec.pop("provider"),
                     model=spec.pop("model"), base_url=prov["base_url"].rstrip("/"),
                     api_key_env=key_env, options=spec,
                     key=None if key_env else "")  # a provider with no api_key_env needs no key

    def resolve(self, preset: str) -> dict[str, Stage]:
        if preset not in self.presets:
            raise ConfigError(f"Unknown preset '{preset}'. Available: {', '.join(self.presets)}")
        stages = {}
        for kind in KINDS:
            model_id = (self.presets[preset] or {}).get(kind)
            if not isinstance(model_id, str):
                raise ConfigError(f"Preset '{preset}' in config/models.yaml needs a {kind}: <catalog id> line.")
            stages[kind] = self.stage(kind, model_id)
        return stages

    def missing_keys(self, preset: str) -> list[str]:
        envs = {s.api_key_env for s in self.resolve(preset).values() if s.key is None}
        return sorted(e for e in envs if not os.environ.get(e, "").strip())

    def rates(self, stage: Stage) -> dict | None:
        if stage.own_rates is not None:
            return stage.own_rates
        return (self.pricing.get(stage.provider) or {}).get(stage.model)
