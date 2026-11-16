//! Cost calculation. All amounts are USD.
//!
//! Each function returns (cost, source) where source is:
//!   "api"          - the provider reported the cost in its response
//!   "pricing.yaml" - calculated from the configured rate
//!   "no-rate"      - no rate configured for this model; cost counted as 0
use serde_json::Value;

fn rate(rates: Option<&Value>, key: &str) -> Option<f64> {
    rates.and_then(|r| r.get(key)).and_then(|v| v.as_f64())
}

pub fn llm_cost(rates: Option<&Value>, input_tokens: u64, output_tokens: u64, api_cost: Option<f64>) -> (f64, String) {
    if let Some(c) = api_cost {
        return (c, "api".into());
    }
    match rate(rates, "input_per_million") {
        Some(i) => {
            let o = rate(rates, "output_per_million").unwrap_or(0.0);
            ((input_tokens as f64 * i + output_tokens as f64 * o) / 1_000_000.0, "pricing.yaml".into())
        }
        None => (0.0, "no-rate".into()),
    }
}

pub fn stt_cost(rates: Option<&Value>, seconds: f64, api_cost: Option<f64>) -> (f64, String) {
    if let Some(c) = api_cost {
        return (c, "api".into());
    }
    match rate(rates, "per_minute") {
        Some(r) => (seconds / 60.0 * r, "pricing.yaml".into()),
        None => (0.0, "no-rate".into()),
    }
}

pub fn tts_cost(rates: Option<&Value>, characters: usize) -> (f64, String) {
    match rate(rates, "per_1k_characters") {
        Some(r) => (characters as f64 / 1000.0 * r, "pricing.yaml".into()),
        None => (0.0, "no-rate".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn llm() {
        let (c, s) = llm_cost(Some(&json!({"input_per_million": 0.035, "output_per_million": 0.37})), 120, 85, None);
        assert!((c - (120.0 * 0.035 + 85.0 * 0.37) / 1e6).abs() < 1e-15);
        assert_eq!(s, "pricing.yaml");
    }
    #[test]
    fn api_cost_wins() {
        assert_eq!(llm_cost(Some(&json!({"input_per_million": 1})), 10, 10, Some(0.5)), (0.5, "api".to_string()));
        assert_eq!(stt_cost(Some(&json!({"per_minute": 1})), 60.0, Some(0.25)), (0.25, "api".to_string()));
    }
    #[test]
    fn stt_tts_and_missing() {
        assert!((stt_cost(Some(&json!({"per_minute": 0.006})), 30.0, None).0 - 0.003).abs() < 1e-12);
        assert!((tts_cost(Some(&json!({"per_1k_characters": 0.015})), 200).0 - 0.003).abs() < 1e-12);
        assert_eq!(tts_cost(None, 100), (0.0, "no-rate".to_string()));
    }
}
