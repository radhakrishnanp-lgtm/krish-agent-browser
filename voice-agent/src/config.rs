//! Loads and validates the YAML config. Fails early with a clear message.
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::Path;

pub const KINDS: [&str; 3] = ["llm", "stt", "tts"];

/// One pipeline stage (llm / stt / tts) resolved to an endpoint + model.
#[derive(Clone, Debug)]
pub struct Stage {
    pub kind: String,
    pub id: String,
    pub label: String,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub options: Map<String, Value>,
    /// Set for models added in the UI and for providers that need no key: the key itself ("" = none).
    pub key: Option<String>,
    /// Set for models added in the UI: their own price, used instead of pricing.yaml.
    pub own_rates: Option<Value>,
    pub custom: bool,
}

impl Stage {
    /// The key to send, or "" when the endpoint needs none (a local server).
    pub fn api_key(&self) -> Result<String, String> {
        if let Some(k) = &self.key {
            return Ok(k.clone());
        }
        let key = std::env::var(&self.api_key_env).unwrap_or_default().trim().to_string();
        if key.is_empty() {
            return Err(format!(
                "{} is not set. Add it to voice-agent/.env (needed for the {} model '{}').",
                self.api_key_env,
                self.kind.to_uppercase(),
                self.model
            ));
        }
        Ok(key)
    }
    pub fn name(&self) -> &str {
        if self.label.is_empty() { &self.model } else { &self.label }
    }
    pub fn opt_str(&self, key: &str) -> Option<String> {
        self.options.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from)
    }
    pub fn opt_f64(&self, key: &str) -> Option<f64> {
        self.options.get(key).and_then(|v| v.as_f64())
    }
    pub fn needs_env_key(&self) -> bool {
        self.key.is_none()
    }
}

/// Walks nested maps: `get(&v, &["vad", "threshold"])`. Missing keys give Null.
pub fn get<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p).unwrap_or(&Value::Null);
    }
    cur
}

fn money(v: f64) -> String {
    if v == 0.0 {
        return "$0".into();
    }
    let s = format!("{:.5}", v);
    format!("${}", s.trim_end_matches('0').trim_end_matches('.'))
}

/// A short price for the UI, e.g. "$0.14 in / $0.42 out per 1M tokens".
pub fn price_text(kind: &str, rates: Option<&Value>, custom: bool) -> String {
    const FREE: &str = "free (your own endpoint)";
    let rates = match rates {
        Some(r) if r.as_object().map(|o| !o.is_empty()).unwrap_or(false) => r,
        _ => return if custom { FREE.into() } else { "no price set".into() },
    };
    let f = |k: &str| rates.get(k).and_then(|v| v.as_f64());
    match kind {
        "llm" => match f("input_per_million") {
            Some(i) => {
                let o = f("output_per_million").unwrap_or(0.0);
                if i == 0.0 && o == 0.0 { FREE.into() } else { format!("{} in / {} out per 1M tokens", money(i), money(o)) }
            }
            None => "no price set".into(),
        },
        "stt" => match f("per_minute") {
            Some(v) if v == 0.0 => FREE.into(),
            Some(v) => format!("{} per minute", money(v)),
            None => "no price set".into(),
        },
        "tts" => match f("per_1k_characters") {
            Some(v) if v == 0.0 => FREE.into(),
            Some(v) => format!("{} per 1,000 characters", money(v)),
            None => "no price set".into(),
        },
        _ => "no price set".into(),
    }
}

#[derive(Debug)]
pub struct Config {
    pub default: Value,
    pub providers: Value,
    pub catalog: Value,
    pub presets: Value,
    pub pricing: Value,
    pub modes: Map<String, Value>,
    pub default_mode: String,
    pub active_preset: String,
}

fn read(root: &Path, name: &str) -> Result<Value, String> {
    let path = root.join("config").join(name);
    let text = std::fs::read_to_string(&path).map_err(|_| format!("Missing config file: {}", path.display()))?;
    let v: Value = serde_yaml::from_str(&text).map_err(|e| format!("Invalid YAML in {}: {}", path.display(), e))?;
    Ok(if v.is_null() { Value::Object(Map::new()) } else { v })
}

impl Config {
    pub fn load(root: &Path) -> Result<Config, String> {
        let default = read(root, "default.yaml")?;
        let providers = get(&read(root, "providers.yaml")?, &["providers"]).clone();
        let models = read(root, "models.yaml")?;
        let pricing = read(root, "pricing.yaml")?;
        let modes_doc = read(root, "modes.yaml")?;
        let modes = get(&modes_doc, &["modes"]).as_object().cloned().unwrap_or_default();
        if modes.is_empty() {
            return Err("config/modes.yaml defines no modes.".into());
        }
        for (key, m) in &modes {
            if m.get("system_prompt").and_then(|v| v.as_str()).map(|s| s.trim().is_empty()).unwrap_or(true) {
                return Err(format!("Mode '{}' in config/modes.yaml has no system_prompt.", key));
            }
        }
        let default_mode = modes_doc
            .get("default_mode")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| modes.keys().next().cloned().unwrap());
        if !modes.contains_key(&default_mode) {
            return Err(format!("default_mode '{}' is not defined in config/modes.yaml.", default_mode));
        }
        let mut cfg = Config {
            default,
            providers,
            catalog: get(&models, &["catalog"]).clone(),
            presets: get(&models, &["presets"]).clone(),
            pricing,
            modes,
            default_mode,
            active_preset: String::new(),
        };
        for kind in KINDS {
            let ids = cfg.catalog_ids(kind);
            if ids.is_empty() {
                return Err(format!("config/models.yaml has no catalog.{} entries.", kind));
            }
            for id in ids {
                cfg.stage(kind, &id)?; // validate every catalog entry up front
            }
        }
        let presets = cfg.preset_names();
        if presets.is_empty() {
            return Err("config/models.yaml defines no presets.".into());
        }
        for name in &presets {
            cfg.resolve(name)?;
        }
        cfg.active_preset = std::env::var("VOICE_AGENT_PRESET")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| cfg.default.get("active_preset").and_then(|v| v.as_str()).map(String::from))
            .unwrap_or_else(|| "default".into());
        if !presets.contains(&cfg.active_preset) {
            return Err(format!(
                "Preset '{}' not found in config/models.yaml. Available: {}",
                cfg.active_preset,
                presets.join(", ")
            ));
        }
        Ok(cfg)
    }

    pub fn catalog_ids(&self, kind: &str) -> Vec<String> {
        self.catalog.get(kind).and_then(|v| v.as_object()).map(|o| o.keys().cloned().collect()).unwrap_or_default()
    }

    pub fn preset_names(&self) -> Vec<String> {
        self.presets.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default()
    }

    /// Builds a Stage from a catalog entry.
    pub fn stage(&self, kind: &str, model_id: &str) -> Result<Stage, String> {
        let mut spec = get(&self.catalog, &[kind, model_id]).as_object().cloned().unwrap_or_default();
        if spec.is_empty() {
            return Err(format!("No {} model '{}' in the catalog of config/models.yaml.", kind, model_id));
        }
        let take = |spec: &mut Map<String, Value>, k: &str| spec.remove(k).and_then(|v| v.as_str().map(String::from));
        let label = take(&mut spec, "label").unwrap_or_default();
        let (provider, model) = match (take(&mut spec, "provider"), take(&mut spec, "model")) {
            (Some(p), Some(m)) => (p, m),
            _ => return Err(format!("catalog.{}.{} in config/models.yaml needs both provider and model.", kind, model_id)),
        };
        let prov = self.providers.get(&provider).filter(|p| p.get("base_url").and_then(|v| v.as_str()).is_some()).ok_or_else(|| {
            format!("catalog.{}.{} uses provider '{}', which is not defined in config/providers.yaml.", kind, model_id, provider)
        })?;
        let key_env = prov.get("api_key_env").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Ok(Stage {
            kind: kind.into(),
            id: model_id.into(),
            label,
            provider,
            model,
            base_url: prov["base_url"].as_str().unwrap().trim_end_matches('/').to_string(),
            key: if key_env.is_empty() { Some(String::new()) } else { None }, // no api_key_env: needs no key
            api_key_env: key_env,
            options: spec,
            own_rates: None,
            custom: false,
        })
    }

    pub fn resolve(&self, preset: &str) -> Result<HashMap<String, Stage>, String> {
        let p = self
            .presets
            .get(preset)
            .ok_or_else(|| format!("Unknown preset '{}'. Available: {}", preset, self.preset_names().join(", ")))?;
        let mut out = HashMap::new();
        for kind in KINDS {
            let id = p
                .get(kind)
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("Preset '{}' in config/models.yaml needs a {}: <catalog id> line.", preset, kind))?;
            out.insert(kind.to_string(), self.stage(kind, id)?);
        }
        Ok(out)
    }

    pub fn missing_keys(&self, preset: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .resolve(preset)
            .map(|s| s.values().filter(|s| s.needs_env_key() && env_missing(&s.api_key_env)).map(|s| s.api_key_env.clone()).collect())
            .unwrap_or_default();
        out.sort();
        out.dedup();
        out
    }

    pub fn rates(&self, stage: &Stage) -> Option<Value> {
        if let Some(r) = &stage.own_rates {
            return Some(r.clone());
        }
        let r = get(&self.pricing, &[&stage.provider, &stage.model]);
        if r.is_null() { None } else { Some(r.clone()) }
    }

    pub fn f64(&self, path: &[&str], fallback: f64) -> f64 {
        get(&self.default, path).as_f64().unwrap_or(fallback)
    }
    pub fn bool(&self, path: &[&str], fallback: bool) -> bool {
        get(&self.default, path).as_bool().unwrap_or(fallback)
    }
    pub fn str(&self, path: &[&str], fallback: &str) -> String {
        get(&self.default, path).as_str().unwrap_or(fallback).to_string()
    }
}

pub fn env_missing(name: &str) -> bool {
    std::env::var(name).unwrap_or_default().trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn price_lines() {
        assert_eq!(price_text("llm", Some(&json!({"input_per_million": 0.14, "output_per_million": 0.42})), false), "$0.14 in / $0.42 out per 1M tokens");
        assert_eq!(price_text("tts", Some(&json!({"per_1k_characters": 0.00062})), false), "$0.00062 per 1,000 characters");
        assert_eq!(price_text("stt", Some(&json!({"per_minute": 0})), true), "free (your own endpoint)");
        assert_eq!(price_text("llm", None, false), "no price set");
    }

    #[test]
    fn shipped_config_is_valid_and_fully_priced() {
        let cfg = Config::load(&root()).expect("the YAML in config/ must load");
        assert!(cfg.modes.contains_key(&cfg.default_mode));
        for name in cfg.preset_names() {
            for (kind, stage) in cfg.resolve(&name).unwrap() {
                assert!(cfg.catalog_ids(&kind).contains(&stage.id), "{name}: {kind}");
            }
        }
        for kind in KINDS {
            for id in cfg.catalog_ids(kind) {
                let stage = cfg.stage(kind, &id).unwrap();
                assert!(cfg.rates(&stage).is_some(), "no price for {} / {}", stage.provider, stage.model);
            }
        }
    }

    #[test]
    fn nested_get() {
        let v = json!({"vad": {"threshold": 0.5}});
        assert_eq!(get(&v, &["vad", "threshold"]).as_f64(), Some(0.5));
        assert!(get(&v, &["vad", "nope", "deeper"]).is_null());
    }
}
