"""Cost calculation. All amounts are USD.

Each function returns (cost, source) where source is:
  "api"          - the provider reported the cost in its response
  "pricing.yaml" - calculated from the configured rate
  "no-rate"      - no rate configured for this model; cost counted as 0
"""
from __future__ import annotations


def llm_cost(rates: dict | None, input_tokens: int, output_tokens: int, api_cost: float | None = None):
    if api_cost is not None:
        return float(api_cost), "api"
    if not rates or "input_per_million" not in rates:
        return 0.0, "no-rate"
    cost = (input_tokens * rates["input_per_million"] + output_tokens * rates.get("output_per_million", 0)) / 1_000_000
    return cost, "pricing.yaml"


def stt_cost(rates: dict | None, seconds: float, api_cost: float | None = None):
    if api_cost is not None:
        return float(api_cost), "api"
    if not rates or "per_minute" not in rates:
        return 0.0, "no-rate"
    return seconds / 60.0 * rates["per_minute"], "pricing.yaml"


def tts_cost(rates: dict | None, characters: int):
    if not rates or "per_1k_characters" not in rates:
        return 0.0, "no-rate"
    return characters / 1000.0 * rates["per_1k_characters"], "pricing.yaml"
