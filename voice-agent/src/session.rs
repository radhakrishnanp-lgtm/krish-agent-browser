//! One browser connection: the turn pipeline (STT -> LLM -> TTS), cost tracking, the
//! per-connection debug log, and the saved conversation it is attached to.
use crate::config::{env_missing, price_text, Config, Stage, KINDS};
use crate::pricing;
use crate::providers::{self, ProviderError, SttResult, TTS_SAMPLE_RATE};
use crate::store::Store;
use crate::vad::{TurnDetector, Vad, VadEvent, SAMPLE_RATE};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Instant;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

const MAX_SAVED_EVENTS: usize = 2000;
const MIN_TTS_CHARS: usize = 12; // shorter sentences ("Hi!") are merged with the next one
const FIRST_CLAUSE_CHARS: usize = 60; // a long first sentence is split at a comma so audio starts sooner
const TTS_PARALLEL: usize = 3; // sentences synthesised at once (played back in order)
const TITLE_CHARS: usize = 48;
const MAX_INSTRUCTIONS: usize = 2000;
const SPOKEN_RULES: &str = "Your replies are spoken aloud, so keep them short and conversational: one to three \
                            sentences unless asked for more. No markdown, no lists, no emojis.";

static SENTENCE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[.!?。！？]["')\]]*\s+|\n+"#).unwrap());
static CLAUSE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[,;:]\s+|\s+[–—-]\s+").unwrap());
static HTTP_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://[^\s/]+").unwrap());

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

fn round8(v: f64) -> f64 {
    (v * 1e8).round() / 1e8
}

fn squeeze(s: &str, max: usize) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

fn text_of(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Splits finished sentences off the front of `pending`. A long first sentence is also
/// split at a clause break, so the first audio starts sooner.
fn take_sentences(pending: &mut String, sent_any: &mut bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut cut = 0;
    for m in SENTENCE_END.find_iter(pending) {
        let s = pending[cut..m.end()].trim();
        if s.chars().count() >= MIN_TTS_CHARS {
            out.push(s.to_string());
            cut = m.end();
        }
    }
    pending.drain(..cut);
    if !out.is_empty() {
        *sent_any = true;
    } else if !*sent_any && pending.chars().count() >= FIRST_CLAUSE_CHARS {
        if let Some(m) = CLAUSE_END.find_iter(pending).last() {
            if pending[..m.start()].chars().count() >= MIN_TTS_CHARS * 2 {
                let s = pending[..m.end()].trim().trim_end_matches(['–', '—', '-']).trim().to_string();
                let end = m.end();
                pending.drain(..end);
                out.push(s);
                *sent_any = true;
            }
        }
    }
    out
}

/// What goes to the browser: JSON messages and raw reply audio.
pub enum Out {
    Json(Value),
    Audio(Vec<u8>),
}

/// Shared by every connection.
pub struct Shared {
    pub root: PathBuf,
    pub cfg: RwLock<Arc<Config>>,
    pub store: Store,
    pub http: reqwest::Client,
    pub vad: Vad,
    pub warmed: Mutex<HashMap<String, Instant>>,
}

impl Shared {
    pub fn config(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
    }
    /// Re-reads the YAML so edits apply without a restart. On a mistake the old config stays.
    pub fn reload(&self) -> Result<Arc<Config>, String> {
        let fresh = Arc::new(Config::load(&self.root)?);
        *self.cfg.write().unwrap() = fresh.clone();
        Ok(fresh)
    }
}

#[derive(Serialize, Clone)]
struct Turn {
    index: usize,
    time: String,
    conversation_id: String,
    mode: String,
    input: String,
    user_text: String,
    assistant_text: String,
    llm_input_tokens: u64,
    llm_output_tokens: u64,
    llm_cost: f64,
    stt_seconds: f64,
    stt_cost: f64,
    tts_characters: usize,
    tts_cost: f64,
    turn_total: f64,
    cost_sources: Map<String, Value>,
    latency_ms: Map<String, Value>,
    interrupted: bool,
    errors: Vec<String>,
    #[serde(skip)]
    audio: Vec<u8>, // the reply's audio, kept for the recording
    #[serde(skip)]
    t0: Option<Instant>,
    #[serde(skip)]
    user_saved: bool,
    #[serde(skip)]
    llm_billed: bool,
    #[serde(skip)]
    llm_prompt_chars: usize,
}

type TurnRef = Arc<Mutex<Turn>>;

struct TurnHandle {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

pub struct TurnInput {
    pub pcm: Option<Vec<u8>>,
    pub text: Option<String>,
    pub greeting: Option<String>,
    pub stt: Option<JoinHandle<Result<SttResult, ProviderError>>>,
}

enum Outcome {
    Done,
    Cancelled,
    Failed(ProviderError),
}

enum TtsItem {
    Audio(Vec<u8>),
    Done,
    Failed(ProviderError),
}

struct State {
    cfg: Arc<Config>,
    preset: String,
    stages: HashMap<String, Stage>,
    custom_allowed: bool,
    conv: Option<Value>,
    turns: Vec<Value>,
    turn_count: usize,
    totals: HashMap<&'static str, f64>,
    show_raw: bool,
    user_voice: Option<String>,
    user_speed: Option<f64>,
    user_pause_ms: Option<f64>,
    record: bool,
    record_allowed: bool,
    record_cap_mb: f64,
    cap_warned: bool,
    default_threshold: f64,
    barge_in: bool,
    default_silence_ms: f64,
    min_silence_ms: f64,
    budget: f64,
    orphan_stt_cost: f64,
    vad: TurnDetector,
    turn: Option<TurnHandle>,
    review: Option<JoinHandle<()>>,
    early: Option<JoinHandle<Result<SttResult, ProviderError>>>,
}

pub struct Session {
    pub sh: Arc<Shared>,
    out: UnboundedSender<Out>,
    st: Mutex<State>,
    events: Mutex<Vec<Value>>,
    pub id: String,
    start_time: String,
    pub log_path: PathBuf,
}

fn zero_totals() -> HashMap<&'static str, f64> {
    HashMap::from([("llm", 0.0), ("stt", 0.0), ("tts", 0.0), ("total", 0.0)])
}

fn totals_json(t: &HashMap<&'static str, f64>) -> Value {
    json!({"llm": round8(t["llm"]), "stt": round8(t["stt"]), "tts": round8(t["tts"]), "total": round8(t["total"])})
}

fn conv_totals_json(conv: &Value) -> Value {
    let f = |k: &str| round8(conv["totals"][k].as_f64().unwrap_or(0.0));
    json!({"llm": f("llm"), "stt": f("stt"), "tts": f("tts"), "total": f("total")})
}

fn kind_key(kind: &str) -> &'static str {
    match kind {
        "llm" => "llm",
        "stt" => "stt",
        _ => "tts",
    }
}

fn config_error(message: String) -> ProviderError {
    ProviderError { message, raw: String::new() }
}

impl State {
    /// Modes from config/modes.yaml plus the ones created in the UI.
    fn all_modes(&self, store: &Store) -> Map<String, Value> {
        let mut modes = self.cfg.modes.clone();
        for (key, mut m) in store.custom_modes() {
            let instructions = m.get("instructions").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            m["system_prompt"] = json!(format!("{}\n\n{}", instructions, SPOKEN_RULES));
            modes.insert(key, m);
        }
        modes
    }

    fn mode(&self, store: &Store) -> Value {
        let modes = self.all_modes(store);
        let key = self.conv.as_ref().and_then(|c| c["mode"].as_str()).unwrap_or(&self.cfg.default_mode).to_string();
        modes.get(&key).or_else(|| modes.get(&self.cfg.default_mode)).cloned().unwrap_or(Value::Null)
    }

    /// How long a pause ends the turn: the user's setting, else the mode's, else the default.
    fn apply_pause(&mut self, store: &Store) {
        let mode_ms = self.mode(store).get("min_silence_ms").and_then(|v| v.as_f64()).filter(|v| *v > 0.0);
        self.min_silence_ms = self.user_pause_ms.or(mode_ms).unwrap_or(self.default_silence_ms);
        self.vad.set_min_silence(self.min_silence_ms);
    }

    fn missing_keys(&self) -> Vec<String> {
        let mut out: Vec<String> =
            self.stages.values().filter(|s| s.needs_env_key() && env_missing(&s.api_key_env)).map(|s| s.api_key_env.clone()).collect();
        out.sort();
        out.dedup();
        out
    }

    fn models(&self) -> Value {
        json!({"llm": self.stages["llm"].model, "stt": self.stages["stt"].model, "tts": self.stages["tts"].model})
    }

    fn stage_for(&self, store: &Store, kind: &str, model_id: &str) -> Result<Stage, String> {
        if !model_id.starts_with("custom-") {
            return self.cfg.stage(kind, model_id);
        }
        let models = store.custom_models();
        let spec = models
            .get(model_id)
            .filter(|s| self.custom_allowed && s.get("kind").and_then(|k| k.as_str()) == Some(kind))
            .ok_or_else(|| format!("Your own {} model '{}' no longer exists.", kind, model_id))?;
        let mut options = Map::new();
        for k in ["voice", "voices", "format", "language", "max_tokens"] {
            if let Some(v) = spec.get(k).filter(|v| !v.is_null()) {
                options.insert(k.to_string(), v.clone());
            }
        }
        Ok(Stage {
            kind: kind.into(),
            id: model_id.into(),
            label: text_of(spec, "label"),
            provider: "custom".into(),
            model: text_of(spec, "model"),
            base_url: text_of(spec, "base_url"),
            api_key_env: String::new(),
            options,
            key: Some(text_of(spec, "api_key")),
            own_rates: Some(spec.get("rates").cloned().unwrap_or_else(|| json!({}))),
            custom: true,
        })
    }

    /// Every model the UI can offer, with a price line. API keys are never included.
    fn catalog(&self, store: &Store) -> Value {
        let mut out = Map::new();
        let custom = if self.custom_allowed { store.custom_models() } else { Map::new() };
        for kind in KINDS {
            let mut items = Vec::new();
            for id in self.cfg.catalog_ids(kind) {
                if let Ok(s) = self.cfg.stage(kind, &id) {
                    items.push(json!({"id": id, "label": s.name(), "model": s.model, "provider": s.provider,
                        "price": price_text(kind, self.cfg.rates(&s).as_ref(), false), "custom": false,
                        "ready": !s.needs_env_key() || !env_missing(&s.api_key_env)}));
                }
            }
            for (id, spec) in &custom {
                if spec.get("kind").and_then(|k| k.as_str()) == Some(kind) {
                    let model = text_of(spec, "model");
                    let label = Some(text_of(spec, "label")).filter(|l| !l.is_empty()).unwrap_or_else(|| model.clone());
                    items.push(json!({"id": id, "label": label, "model": model, "provider": text_of(spec, "base_url"),
                        "price": price_text(kind, spec.get("rates"), true), "custom": true, "ready": true}));
                }
            }
            out.insert(kind.to_string(), Value::Array(items));
        }
        Value::Object(out)
    }

    fn hello(&self, store: &Store, session_id: &str) -> Value {
        let tts = &self.stages["tts"];
        let voices = match tts.options.get("voices").and_then(|v| v.as_array()).filter(|v| !v.is_empty()) {
            Some(v) => Value::Array(v.clone()),
            None => tts.opt_str("voice").map(|v| json!([v])).unwrap_or_else(|| json!([])),
        };
        let modes: Vec<Value> = self
            .all_modes(store)
            .iter()
            .map(|(k, m)| {
                json!({"id": k, "name": m.get("name").and_then(|v| v.as_str()).unwrap_or(k),
                    "description": m.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    "custom": m.get("custom").and_then(|v| v.as_bool()).unwrap_or(false),
                    "review": m.get("review_prompt").and_then(|v| v.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false)})
            })
            .collect();
        json!({
            "type": "hello",
            "session_id": session_id,
            "preset": self.preset,
            "presets": self.cfg.preset_names(),
            "models": self.models(),
            "modes": modes,
            "recording_allowed": self.record_allowed,
            "instructions": store.instructions(),
            "default_mode": self.cfg.default_mode,
            "voices": voices,
            "default_voice": tts.opt_str("voice"),
            "budget_usd": self.budget,
            "barge_in": self.barge_in,
            "input_sample_rate": SAMPLE_RATE,
            "output_sample_rate": TTS_SAMPLE_RATE,
            "missing_keys": self.missing_keys(),
            "catalog": self.catalog(store),
            "selected": {"llm": self.stages["llm"].id, "stt": self.stages["stt"].id, "tts": self.stages["tts"].id},
            "custom_models_allowed": self.custom_allowed,
        })
    }

    fn cost_msg(&self, turn: Option<&Turn>) -> Value {
        json!({
            "type": "cost",
            "turn": turn.map(|t| json!({"index": t.index, "llm_cost": t.llm_cost, "stt_cost": t.stt_cost, "tts_cost": t.tts_cost,
                "turn_total": t.turn_total, "llm_input_tokens": t.llm_input_tokens, "llm_output_tokens": t.llm_output_tokens,
                "stt_seconds": t.stt_seconds, "tts_characters": t.tts_characters})),
            "session": totals_json(&self.totals),
            "conversation": self.conv.as_ref().map(conv_totals_json),
            "budget_usd": self.budget,
        })
    }

    fn conversation_msg(&self) -> Value {
        json!({"type": "conversation", "conversation": self.conv.as_ref().map(|c| json!({
            "id": c["id"], "title": c["title"], "mode": c["mode"], "messages": c["messages"],
            "review": c.get("review").cloned().unwrap_or(Value::Null),
            "user_seconds": (c.get("user_seconds").and_then(|v| v.as_f64()).unwrap_or(0.0) * 10.0).round() / 10.0,
            "instructions": c.get("instructions").and_then(|v| v.as_str()).unwrap_or(""),
            "totals": conv_totals_json(c)}))})
    }

    fn busy(&self) -> bool {
        self.turn.as_ref().map(|t| !t.handle.is_finished()).unwrap_or(false)
    }
}

impl Session {
    pub fn new(sh: Arc<Shared>, out: UnboundedSender<Out>) -> Result<Arc<Session>, String> {
        let cfg = sh.reload()?; // YAML edits apply to every new connection without a restart
        let preset = cfg.active_preset.clone();
        let stages = cfg.resolve(&preset)?;
        let id = format!("{}-{}", chrono::Local::now().format("%Y-%m-%d-%H-%M-%S"), &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let min_silence = cfg.f64(&["vad", "min_silence_ms"], 550.0);
        let threshold = cfg.f64(&["vad", "threshold"], 0.5);
        let record = cfg.bool(&["recording", "enabled"], true);
        let state = State {
            vad: TurnDetector::new(
                threshold,
                min_silence,
                cfg.f64(&["vad", "min_speech_ms"], 250.0),
                cfg.f64(&["vad", "pre_roll_ms"], 300.0),
                cfg.f64(&["vad", "max_utterance_s"], 30.0),
                cfg.f64(&["vad", "early_stt_ms"], 250.0),
            ),
            preset,
            stages,
            custom_allowed: cfg.bool(&["custom_models", "enabled"], true),
            conv: None,
            turns: Vec::new(),
            turn_count: 0,
            totals: zero_totals(),
            show_raw: false,
            user_voice: None,
            user_speed: None,
            user_pause_ms: None,
            record,
            record_allowed: record,
            record_cap_mb: cfg.f64(&["recording", "max_total_mb"], 0.0),
            cap_warned: false,
            default_threshold: threshold,
            barge_in: cfg.bool(&["vad", "barge_in"], true),
            default_silence_ms: min_silence,
            min_silence_ms: min_silence,
            budget: cfg.f64(&["cost", "session_budget_usd"], 0.0),
            orphan_stt_cost: 0.0,
            turn: None,
            review: None,
            early: None,
            cfg: cfg.clone(),
        };
        let log_path = sh.root.join(cfg.str(&["logging", "sessions_dir"], "logs/sessions")).join(format!("{}.json", id));
        Ok(Arc::new(Session { sh, out, st: Mutex::new(state), events: Mutex::new(Vec::new()), id, start_time: now_iso(), log_path }))
    }

    // ---------- outbound helpers ----------

    pub fn send(&self, msg: Value) {
        let _ = self.out.send(Out::Json(msg));
    }

    pub fn log(&self, level: &str, message: &str, data: Option<Value>) {
        let mut entry = json!({"ts": now_iso(), "level": level, "message": message});
        if let Some(d) = data {
            entry["data"] = d;
        }
        crate::log(level, &format!("[{}] {}", &self.id[self.id.len() - 8..], message));
        {
            let mut events = self.events.lock().unwrap();
            events.push(entry.clone());
            let extra = events.len().saturating_sub(MAX_SAVED_EVENTS);
            events.drain(..extra);
        }
        entry["type"] = json!("log");
        self.send(entry);
    }

    fn raw(&self, stage: &str, payload: Value) {
        if self.st.lock().unwrap().show_raw {
            self.send(json!({"type": "raw", "ts": now_iso(), "stage": stage, "payload": payload}));
        }
    }

    fn store(&self) -> &Store {
        &self.sh.store
    }

    pub fn send_hello(&self) {
        let msg = self.st.lock().unwrap().hello(self.store(), &self.id);
        self.send(msg);
    }

    pub fn missing_keys(&self) -> Vec<String> {
        self.st.lock().unwrap().missing_keys()
    }

    pub fn preset(&self) -> String {
        self.st.lock().unwrap().preset.clone()
    }

    pub fn emit_cost(&self) {
        let msg = self.st.lock().unwrap().cost_msg(None);
        self.send(msg);
    }

    pub fn warm(self: &Arc<Self>) {
        let mut urls: Vec<String> = self.st.lock().unwrap().stages.values().map(|s| s.base_url.clone()).collect();
        urls.sort();
        urls.dedup();
        let sh = self.sh.clone();
        tokio::spawn(async move { providers::warm(sh.http.clone(), urls, &sh.warmed).await });
    }

    pub fn set_show_raw(&self, on: bool) {
        self.st.lock().unwrap().show_raw = on;
        self.log("info", &format!("Raw API responses {}", if on { "on" } else { "off" }), None);
    }

    // ---------- models ----------

    /// Switch any single model: {"llm": id, "stt": id, "tts": id}; missing kinds are kept.
    pub fn set_models(self: &Arc<Self>, data: &Value, announce: bool) {
        let mut changed = Vec::new();
        let mut warnings = Vec::new();
        let (hello, summary, models, missing) = {
            let mut st = self.st.lock().unwrap();
            for kind in KINDS {
                let Some(id) = data.get(kind).and_then(|v| v.as_str()) else { continue };
                if st.stages[kind].id == id {
                    continue;
                }
                match st.stage_for(self.store(), kind, id) {
                    Ok(stage) => {
                        st.stages.insert(kind.to_string(), stage);
                        changed.push(kind);
                    }
                    Err(e) => warnings.push(e),
                }
            }
            if changed.contains(&"tts") {
                st.user_voice = None; // voices differ between TTS models
            }
            let summary = KINDS.iter().map(|k| format!("{} {}", k.to_uppercase(), st.stages[*k].name())).collect::<Vec<_>>().join(", ");
            (st.hello(self.store(), &self.id), summary, st.models(), st.missing_keys())
        };
        for w in warnings {
            self.log("warn", &w, None);
        }
        if !changed.is_empty() || announce {
            self.send(hello);
        }
        if !changed.is_empty() {
            self.log("info", &format!("Models now: {}", summary), Some(models));
            for key in missing {
                self.log("error", &format!("{} is not set on the server, so that model will fail. Add it to voice-agent/.env and restart.", key), None);
            }
            self.warm();
        }
    }

    pub fn add_custom_model(self: &Arc<Self>, data: &Value) {
        if !self.st.lock().unwrap().custom_allowed {
            self.log("error", "Adding your own models is turned off on this server (custom_models.enabled).", None);
            return;
        }
        let kind = text_of(data, "kind");
        let label = squeeze(&text_of(data, "label"), 60);
        let base_url: String = text_of(data, "base_url").trim().trim_end_matches('/').chars().take(300).collect();
        let model: String = text_of(data, "model").trim().chars().take(200).collect();
        if !KINDS.contains(&kind.as_str()) || model.is_empty() || !HTTP_URL.is_match(&base_url) {
            self.log("error", "A model needs a type, a model name and an address that starts with http:// or https://.", None);
            return;
        }
        let price = |name: &str| -> f64 {
            let v = match data.get(name) {
                Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
                Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
                _ => 0.0,
            };
            if v.is_finite() { v.max(0.0) } else { 0.0 }
        };
        let rates = match kind.as_str() {
            "llm" => json!({"input_per_million": price("price_in"), "output_per_million": price("price_out")}),
            "stt" => json!({"per_minute": price("price_in")}),
            _ => json!({"per_1k_characters": price("price_in")}),
        };
        let shown = if label.is_empty() { model.clone() } else { label };
        let mut spec = json!({"kind": kind, "label": shown, "base_url": base_url, "model": model,
            "api_key": text_of(data, "api_key").trim().chars().take(400).collect::<String>(), "rates": rates});
        if kind == "tts" {
            let voices: Vec<String> =
                text_of(data, "voices").split(',').map(|v| v.trim().chars().take(80).collect::<String>()).filter(|v| !v.is_empty()).take(40).collect();
            if !voices.is_empty() {
                spec["voice"] = json!(voices[0]);
                spec["voices"] = json!(voices);
            }
            spec["format"] = json!(if text_of(data, "format") == "wav" { "wav" } else { "pcm" });
        }
        if kind == "llm" {
            spec["max_tokens"] = json!(400);
        }
        let id = match self.store().save_custom_model(spec) {
            Ok(id) => id,
            Err(e) => {
                self.log("error", &format!("Could not save the model: {}", e), None);
                return;
            }
        };
        self.log("info", &format!("Added your {} model '{}' at {}", kind.to_uppercase(), shown, base_url), None);
        self.set_models(&json!({kind.clone(): id}), true);
        self.spawn_test(kind, id);
    }

    pub fn delete_custom_model(self: &Arc<Self>, model_id: &str) {
        let Some(spec) = self.store().custom_models().get(model_id).cloned() else { return };
        if !self.store().delete_custom_model(model_id).unwrap_or(false) {
            return;
        }
        let hello = {
            let mut st = self.st.lock().unwrap();
            let kind = text_of(&spec, "kind");
            if st.stages.get(&kind).map(|s| s.id == model_id).unwrap_or(false) {
                // fall back to the preset's model
                if let Ok(mut stages) = st.cfg.resolve(&st.preset) {
                    if let Some(s) = stages.remove(&kind) {
                        st.stages.insert(kind, s);
                    }
                }
            }
            st.hello(self.store(), &self.id)
        };
        self.log("info", &format!("Removed your model '{}'", text_of(&spec, "label")), None);
        self.send(hello);
    }

    pub fn spawn_test(self: &Arc<Self>, kind: String, model_id: String) {
        let me = self.clone();
        tokio::spawn(async move { me.test_model(&kind, &model_id).await });
    }

    /// One small real call, so a wrong address or model name shows up immediately.
    async fn test_model(self: &Arc<Self>, kind: &str, model_id: &str) {
        let t = Instant::now();
        let stage = self.st.lock().unwrap().stage_for(self.store(), kind, model_id);
        let result: Result<(Stage, String), ProviderError> = async {
            let stage = stage.map_err(config_error)?;
            let http = &self.sh.http;
            let detail = match kind {
                "llm" => {
                    let mut text = String::new();
                    let messages = json!([{"role": "user", "content": "Reply with the single word: ready"}]);
                    providers::chat_stream(http, &stage, &messages, Some(20), |d| text.push_str(d)).await?;
                    if text.trim().is_empty() {
                        return Err(providers::error(&stage, "it returned an empty reply"));
                    }
                    format!("it answered \"{}\"", text.trim().chars().take(40).collect::<String>())
                }
                "tts" => {
                    let mut n = 0usize;
                    providers::synthesize(http, &stage, "This is a test.", None, None, |a| n += a.len()).await?;
                    if n == 0 {
                        return Err(providers::error(&stage, "it returned no audio"));
                    }
                    format!("it produced {:.1} s of audio", n as f64 / 2.0 / TTS_SAMPLE_RATE as f64)
                }
                _ => {
                    let res = providers::transcribe(http.clone(), stage.clone(), vec![0u8; SAMPLE_RATE * 2], SAMPLE_RATE as u32, None).await?;
                    format!("it accepted one second of silence and returned \"{}\"", res.text.chars().take(40).collect::<String>())
                }
            };
            Ok((stage, detail))
        }
        .await;
        match result {
            Err(e) => {
                self.log("error", &format!("Model test failed. {}", e), None);
                self.send(json!({"type": "model_test", "id": model_id, "ok": false, "message": e.message}));
            }
            Ok((stage, detail)) => {
                let ms = t.elapsed().as_millis();
                self.log("info", &format!("Model test passed for {}: {}, in {} ms", stage.name(), detail, ms), None);
                self.send(json!({"type": "model_test", "id": model_id, "ok": true,
                    "message": format!("{} works: {}, in {} ms.", stage.name(), detail, ms)}));
            }
        }
    }

    pub fn set_preset(self: &Arc<Self>, name: &str) {
        let result = self.sh.reload().and_then(|cfg| cfg.resolve(name).map(|s| (cfg, s)));
        let (cfg, stages) = match result {
            Ok(v) => v,
            Err(e) => {
                self.log("error", &format!("Could not switch preset: {}", e), None);
                return;
            }
        };
        let (hello, models, missing) = {
            let mut st = self.st.lock().unwrap();
            st.cfg = cfg;
            st.stages = stages;
            st.preset = name.to_string();
            st.user_voice = None; // voices differ between TTS models
            (st.hello(self.store(), &self.id), st.models(), st.missing_keys())
        };
        self.send(hello);
        self.log("info", &format!("Preset switched to '{}'", name), Some(models));
        if !missing.is_empty() {
            self.log("error", &format!("Missing API key(s) for this preset: {}. Add them to voice-agent/.env and restart.", missing.join(", ")), None);
        }
        self.warm();
    }

    pub fn apply_settings(&self, data: &Value) {
        let num = |key: &str| -> Option<f64> {
            match data.get(key) {
                Some(Value::Number(n)) => n.as_f64(),
                Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
            .filter(|v| v.is_finite() && *v > 0.0)
        };
        let mut st = self.st.lock().unwrap();
        st.record = st.record_allowed && data.get("record").and_then(|v| v.as_bool()) != Some(false);
        st.user_pause_ms = num("pause_ms").map(|v| v.clamp(300.0, 3000.0));
        st.apply_pause(self.store());
        let threshold = num("sensitivity").map(|v| v.clamp(0.15, 0.9)).unwrap_or(st.default_threshold);
        st.vad.set_threshold(threshold);
        let voice = data.get("voice").and_then(|v| v.as_str());
        let allowed = st.stages["tts"].options.get("voices").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        st.user_voice = voice.filter(|v| allowed.iter().any(|a| a.as_str() == Some(*v))).map(String::from);
        st.user_speed = num("speed").map(|v| v.clamp(0.7, 1.3));
    }

    // ---------- instructions and modes ----------

    /// Standing instructions for every conversation, and extra ones for the open conversation.
    pub fn set_instructions(&self, data: &Value) {
        let clip = |v: &Value| -> String { v.as_str().unwrap_or("").trim().chars().take(MAX_INSTRUCTIONS).collect() };
        let mut failed = None;
        if let Some(g) = data.get("global") {
            if let Err(e) = self.store().save_instructions(&clip(g)) {
                failed = Some(e.to_string());
            }
        }
        let here = {
            let mut st = self.st.lock().unwrap();
            if let (Some(c), Some(conv)) = (data.get("conversation"), st.conv.as_mut()) {
                conv["instructions"] = json!(clip(c));
                if let Err(e) = self.store().save(conv) {
                    failed = Some(e.to_string());
                }
            }
            st.conv.as_ref().map(|c| text_of(c, "instructions")).unwrap_or_default()
        };
        if let Some(e) = failed {
            self.log("error", &format!("Could not save the instructions: {}", e), None);
            return;
        }
        self.send(json!({"type": "instructions", "global": self.store().instructions(), "conversation": here}));
        self.log("info", "Instructions saved. They apply from your next message.", None);
    }

    pub fn save_custom_mode(&self, data: &Value) {
        let name = squeeze(&text_of(data, "name"), 40);
        let instructions: String = text_of(data, "instructions").trim().chars().take(2000).collect();
        let greeting = squeeze(&text_of(data, "greeting"), 200);
        if name.is_empty() || instructions.chars().count() < 10 {
            self.log("error", "A mode needs a name and at least a sentence of instructions.", None);
            return;
        }
        match self.store().save_custom_mode(&name, &instructions, &greeting) {
            Ok(id) => {
                self.log("info", &format!("Created mode '{}' ({})", name, id), None);
                self.send_hello();
            }
            Err(e) => self.log("error", &format!("Could not save the mode: {}", e), None),
        }
    }

    pub fn delete_custom_mode(&self, mode_id: &str) {
        if self.store().delete_custom_mode(mode_id).unwrap_or(false) {
            self.log("info", &format!("Deleted mode {}", mode_id), None);
            self.send_hello();
        }
    }

    // ---------- conversations and folders ----------

    pub fn send_conversations(&self) {
        self.send(json!({"type": "conversations", "items": self.store().summaries(), "folders": self.store().folders()}));
    }

    fn send_conversation(&self) {
        let msg = self.st.lock().unwrap().conversation_msg();
        self.send(msg);
    }

    pub async fn new_conversation(self: &Arc<Self>, mode: Option<&str>, greet: bool) {
        self.cancel_turn().await;
        let (conv_msg, cost, id, mode, greeting) = {
            let mut st = self.st.lock().unwrap();
            let modes = st.all_modes(self.store());
            let mode = mode.filter(|m| modes.contains_key(*m)).unwrap_or(&st.cfg.default_mode).to_string();
            let conv = self.store().create(&mode);
            let id = text_of(&conv, "id");
            st.conv = Some(conv);
            st.vad.reset();
            st.apply_pause(self.store());
            let greeting = modes.get(&mode).and_then(|m| m.get("greeting")).and_then(|g| g.as_str()).unwrap_or("").trim().to_string();
            (st.conversation_msg(), st.cost_msg(None), id, mode, greeting)
        };
        self.send(conv_msg);
        self.send(cost);
        self.log("info", &format!("New conversation {} in mode '{}'", id, mode), None);
        if greet && !greeting.is_empty() {
            self.start_turn(TurnInput { pcm: None, text: None, greeting: Some(greeting), stt: None });
        }
    }

    pub async fn open_conversation(self: &Arc<Self>, conv_id: &str) {
        self.cancel_turn().await;
        let Some(conv) = self.store().load(conv_id) else {
            self.store().forget(conv_id); // its file is gone: drop it from the list too
            self.st.lock().unwrap().conv = None;
            self.send_conversation();
            self.send_conversations();
            self.log("warn", "That conversation no longer exists.", None);
            return;
        };
        let (conv_msg, cost, missing_mode, line) = {
            let mut st = self.st.lock().unwrap();
            let mode = text_of(&conv, "mode");
            let missing_mode = (!st.all_modes(self.store()).contains_key(&mode)).then(|| (mode, st.cfg.default_mode.clone()));
            let line = format!(
                "Resumed conversation {} \"{}\" ({} messages)",
                conv_id,
                text_of(&conv, "title"),
                conv["messages"].as_array().map(|m| m.len()).unwrap_or(0)
            );
            st.conv = Some(conv);
            st.vad.reset();
            st.apply_pause(self.store());
            (st.conversation_msg(), st.cost_msg(None), missing_mode, line)
        };
        if let Some((mode, fallback)) = missing_mode {
            self.log("warn", &format!("Mode '{}' no longer exists; using '{}'.", mode, fallback), None);
        }
        self.send(conv_msg);
        self.send(cost);
        self.log("info", &line, None);
    }

    pub async fn close_conversation(self: &Arc<Self>) {
        self.cancel_turn().await;
        self.st.lock().unwrap().conv = None;
        self.send_conversation();
        self.emit_cost();
    }

    pub async fn delete_conversation(self: &Arc<Self>, conv_id: &str) {
        let open = self.st.lock().unwrap().conv.as_ref().map(|c| c["id"] == conv_id).unwrap_or(false);
        if open {
            self.cancel_turn().await;
            self.st.lock().unwrap().conv = None;
            self.send_conversation();
        }
        if self.store().delete(conv_id) {
            self.log("info", &format!("Deleted conversation {}", conv_id), None);
        }
        self.send_conversations();
    }

    pub fn rename_conversation(&self, conv_id: &str, title: &str) {
        let title = squeeze(title, 80);
        if title.is_empty() {
            return;
        }
        let mut meta = None;
        let saved = {
            let mut st = self.st.lock().unwrap();
            match st.conv.as_mut().filter(|c| c["id"] == conv_id) {
                Some(conv) => {
                    conv["title"] = json!(title);
                    conv["titled"] = json!(true);
                    meta = Some(json!({"type": "conversation_meta", "id": conv_id, "title": title}));
                    self.store().save(conv)
                }
                None => match self.store().load(conv_id) {
                    Some(mut conv) => {
                        conv["title"] = json!(title);
                        conv["titled"] = json!(true);
                        self.store().save(&mut conv)
                    }
                    None => return,
                },
            }
        };
        if let Err(e) = saved {
            self.log("error", &format!("Could not save the conversation: {}", e), None);
        }
        if let Some(m) = meta {
            self.send(m);
        }
        self.send_conversations();
    }

    /// create / rename / delete / reorder folders, and move a conversation into one.
    pub fn folder_action(&self, data: &Value) {
        let action = text_of(data, "type");
        let name = squeeze(&text_of(data, "name"), 60);
        let folder_id = data.get(if action == "move" { "folder" } else { "id" }).and_then(|v| v.as_str()).map(String::from);
        let store = self.store();
        let mut warn = None;
        let result: std::io::Result<()> = (|| {
            match (action.as_str(), &folder_id) {
                ("folder_create", _) if !name.is_empty() => {
                    store.create_folder(&name)?;
                }
                ("folder_rename", Some(id)) if !name.is_empty() => {
                    store.rename_folder(id, &name)?;
                }
                ("folder_delete", Some(id)) => {
                    store.delete_folder(id)?;
                    let mut st = self.st.lock().unwrap();
                    if let Some(conv) = st.conv.as_mut().filter(|c| c["folder"] == id.as_str()) {
                        conv["folder"] = Value::Null;
                    }
                }
                ("folder_order", _) => {
                    if let Some(ids) = data.get("ids").and_then(|v| v.as_array()) {
                        store.reorder_folders(ids)?;
                    }
                }
                ("move", folder) => {
                    let conv_id = text_of(data, "id");
                    let valid = folder.as_ref().map(|f| store.has_folder(f)).unwrap_or(true);
                    let open = {
                        // the open conversation is saved from memory later: change it there too
                        let mut st = self.st.lock().unwrap();
                        match st.conv.as_mut().filter(|c| c["id"] == conv_id.as_str()) {
                            Some(conv) => {
                                if valid {
                                    conv["folder"] = folder.as_ref().map(|f| json!(f)).unwrap_or(Value::Null);
                                }
                                true
                            }
                            None => false,
                        }
                    };
                    if !store.move_conversation(&conv_id, folder.as_deref())? && !open {
                        warn = Some("That conversation or folder no longer exists.");
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.log("error", &format!("Could not save the folders: {}", e), None);
        }
        if let Some(w) = warn {
            self.log("warn", w, None);
        }
        self.send_conversations();
    }

    // ---------- turn control ----------

    pub async fn cancel_turn(&self) {
        let turn = self.st.lock().unwrap().turn.take();
        if let Some(t) = turn {
            t.cancel.cancel();
            let _ = t.handle.await;
        }
    }

    pub async fn stop(&self) {
        self.send(json!({"type": "interrupt"}));
        self.cancel_turn().await;
    }

    pub async fn close(self: &Arc<Self>) -> (usize, f64) {
        self.cancel_turn().await;
        self.drop_early();
        let (turns, total) = {
            let mut st = self.st.lock().unwrap();
            if let Some(r) = st.review.take() {
                r.abort();
            }
            (st.turns.len(), st.totals["total"])
        };
        if turns > 0 {
            self.save_log();
        }
        (turns, total)
    }

    /// Discard a speculative transcription (the speaker carried on). Its cost still counts.
    fn drop_early(self: &Arc<Self>) {
        let Some(handle) = self.st.lock().unwrap().early.take() else { return };
        let me = self.clone();
        tokio::spawn(async move {
            if let Ok(Ok(res)) = handle.await {
                let mut st = me.st.lock().unwrap();
                let rates = st.cfg.rates(&st.stages["stt"]);
                st.orphan_stt_cost += pricing::stt_cost(rates.as_ref(), res.seconds, res.api_cost).0;
            }
        });
    }

    fn transcribe_task(&self, pcm: Vec<u8>) -> JoinHandle<Result<SttResult, ProviderError>> {
        let (stage, language) = {
            let st = self.st.lock().unwrap();
            (st.stages["stt"].clone(), st.mode(self.store()).get("stt_language").and_then(|v| v.as_str()).map(String::from))
        };
        tokio::spawn(providers::transcribe(self.sh.http.clone(), stage, pcm, SAMPLE_RATE as u32, language))
    }

    pub async fn on_audio(self: &Arc<Self>, pcm: &[u8]) {
        let vad: &Vad = &self.sh.vad;
        let events = self.st.lock().unwrap().vad.feed(vad, pcm);
        for ev in events {
            let (busy, barge_in) = {
                let st = self.st.lock().unwrap();
                (st.busy(), st.barge_in)
            };
            match ev {
                VadEvent::Start => {
                    self.send(json!({"type": "status", "state": "user_speaking"}));
                    self.warm();
                }
                VadEvent::Confirmed => {
                    if barge_in {
                        self.send(json!({"type": "interrupt"}));
                        if busy {
                            self.log("info", "Barge-in: user spoke, stopping the assistant", None);
                            self.cancel_turn().await;
                        }
                    }
                }
                VadEvent::Pause(audio) => {
                    if !(busy && !barge_in) {
                        self.drop_early();
                        let handle = self.transcribe_task(audio);
                        self.st.lock().unwrap().early = Some(handle);
                    }
                }
                VadEvent::Resume => self.drop_early(),
                VadEvent::Cancel => {
                    self.drop_early();
                    self.send(json!({"type": "status", "state": if busy { "thinking" } else { "listening" }}));
                }
                VadEvent::End(audio) => {
                    if busy && !barge_in {
                        self.drop_early();
                        self.log("warn", "Ignored speech while the assistant was replying (barge_in is off)", None);
                        continue;
                    }
                    let early = self.st.lock().unwrap().early.take();
                    self.start_turn(TurnInput { pcm: Some(audio), text: None, greeting: None, stt: early });
                }
            }
        }
    }

    pub fn start_turn(self: &Arc<Self>, input: TurnInput) {
        let t_start = Instant::now();
        let previous = self.st.lock().unwrap().turn.take();
        if previous.as_ref().map(|p| !p.handle.is_finished()).unwrap_or(false) {
            self.send(json!({"type": "interrupt"}));
        }
        let cancel = CancellationToken::new();
        let (me, token) = (self.clone(), cancel.clone());
        let handle = tokio::spawn(async move {
            if let Some(p) = previous {
                p.cancel.cancel();
                let _ = p.handle.await;
            }
            me.run_turn(input, t_start, token).await;
        });
        self.st.lock().unwrap().turn = Some(TurnHandle { cancel, handle });
    }

    // ---------- the turn pipeline ----------

    fn add_cost(&self, turn: &TurnRef, kind: &str, cost: f64, source: &str) {
        let key = kind_key(kind);
        let mut no_rate = None;
        let msg = {
            let mut st = self.st.lock().unwrap();
            let stage = st.stages[key].clone();
            let source = if stage.custom { source.replace("pricing.yaml", "your price") } else { source.to_string() };
            let mut t = turn.lock().unwrap();
            match key {
                "llm" => t.llm_cost = round8(t.llm_cost + cost),
                "stt" => t.stt_cost = round8(t.stt_cost + cost),
                _ => t.tts_cost = round8(t.tts_cost + cost),
            }
            t.turn_total = round8(t.llm_cost + t.stt_cost + t.tts_cost);
            if source == "no-rate" {
                no_rate = Some(format!("No rate for {} / {} in config/pricing.yaml; its cost is counted as $0", stage.provider, stage.model));
            }
            t.cost_sources.insert(key.to_string(), json!(source));
            *st.totals.get_mut(key).unwrap() += cost;
            *st.totals.get_mut("total").unwrap() += cost;
            if let Some(conv) = st.conv.as_mut().filter(|c| c["id"] == t.conversation_id.as_str()) {
                for k in [key, "total"] {
                    conv["totals"][k] = json!(conv["totals"][k].as_f64().unwrap_or(0.0) + cost);
                }
            }
            st.cost_msg(Some(&t))
        };
        if let Some(w) = no_rate {
            self.log("warn", &w, None);
        }
        self.send(msg);
    }

    async fn run_turn(self: &Arc<Self>, input: TurnInput, t_start: Instant, cancel: CancellationToken) {
        let mut created = None;
        let turn: TurnRef = {
            let mut st = self.st.lock().unwrap();
            if st.budget > 0.0 && st.totals["total"] >= st.budget {
                let (budget, spent) = (st.budget, st.totals["total"]);
                drop(st);
                self.log(
                    "error",
                    &format!(
                        "Session budget of ${:.2} reached (spent ${:.4}). Raise cost.session_budget_usd in config/default.yaml or reload the page for a new session.",
                        budget, spent
                    ),
                    None,
                );
                self.send(json!({"type": "status", "state": "listening"}));
                return;
            }
            if st.conv.is_none() {
                // speaking or typing with no conversation open starts one
                let mode = st.cfg.default_mode.clone();
                st.conv = Some(self.store().create(&mode));
                created = Some(st.conversation_msg());
            }
            st.turn_count += 1;
            let conv = st.conv.as_ref().unwrap();
            Arc::new(Mutex::new(Turn {
                index: st.turn_count,
                time: now_iso(),
                conversation_id: text_of(conv, "id"),
                mode: text_of(conv, "mode"),
                input: if input.greeting.is_some() { "greeting" } else if input.pcm.is_some() { "voice" } else { "text" }.to_string(),
                user_text: input.text.clone().unwrap_or_default(),
                assistant_text: String::new(),
                llm_input_tokens: 0,
                llm_output_tokens: 0,
                llm_cost: 0.0,
                stt_seconds: 0.0,
                stt_cost: 0.0,
                tts_characters: 0,
                tts_cost: 0.0,
                turn_total: 0.0,
                cost_sources: Map::new(),
                latency_ms: Map::new(),
                interrupted: false,
                errors: Vec::new(),
                audio: Vec::new(),
                t0: Some(t_start),
                user_saved: false,
                llm_billed: false,
                llm_prompt_chars: 0,
            }))
        };
        if let Some(msg) = created {
            self.send(msg);
        }
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Outcome::Cancelled,
            r = self.pipeline(&turn, input) => match r {
                Ok(()) => Outcome::Done,
                Err(e) => Outcome::Failed(e),
            },
        };
        self.finalize(&turn, outcome, t_start);
    }

    async fn pipeline(self: &Arc<Self>, turn: &TurnRef, input: TurnInput) -> Result<(), ProviderError> {
        if let Some(pcm) = &input.pcm {
            self.stt(turn, pcm, input.stt).await?;
            if turn.lock().unwrap().user_text.is_empty() {
                self.log("warn", "No words recognised in that audio; turn skipped", None);
                return Ok(());
            }
        }
        if !turn.lock().unwrap().user_text.is_empty() {
            self.record_user(turn, input.pcm.as_deref());
        }
        self.reply(turn, input.greeting).await
    }

    /// Saves the user's words right away so nothing is lost if the reply fails.
    fn record_user(&self, turn: &TurnRef, pcm: Option<&[u8]>) {
        let mut out = Vec::new();
        {
            let mut st = self.st.lock().unwrap();
            let mut t = turn.lock().unwrap();
            let audio = pcm.and_then(|p| self.save_audio(&mut st, &t.conversation_id, "user", p, SAMPLE_RATE as u32));
            let Some(conv) = st.conv.as_mut().filter(|c| c["id"] == t.conversation_id.as_str()) else { return };
            let mut msg = json!({"role": "user", "content": t.user_text, "ts": t.time});
            if let Some(p) = pcm {
                let spoken = conv.get("user_seconds").and_then(|v| v.as_f64()).unwrap_or(0.0) + p.len() as f64 / 2.0 / SAMPLE_RATE as f64;
                conv["user_seconds"] = json!(spoken);
            }
            if let Some(a) = &audio {
                msg["audio"] = json!(a);
            }
            if let Some(list) = conv["messages"].as_array_mut() {
                list.push(msg);
            }
            t.user_saved = true;
            out.push(json!({"type": "transcript", "role": "user", "turn": t.index, "text": t.user_text, "final": true, "audio": audio}));
            if conv.get("titled").and_then(|v| v.as_bool()) != Some(true) && conv["title"] == "New conversation" {
                let flat = squeeze(&t.user_text, usize::MAX);
                let mut title: String = flat.chars().take(TITLE_CHARS).collect::<String>().trim_end().to_string();
                if flat.chars().count() > TITLE_CHARS {
                    title.push('…');
                }
                conv["title"] = json!(title);
                out.push(json!({"type": "conversation_meta", "id": t.conversation_id, "title": title}));
            }
        }
        for m in out {
            self.send(m);
        }
    }

    /// Writes one message's recording. Index = its position in the conversation.
    fn save_audio(&self, st: &mut State, conv_id: &str, role: &str, pcm: &[u8], rate: u32) -> Option<String> {
        if !st.record || pcm.is_empty() {
            return None;
        }
        if st.record_cap_mb > 0.0 && self.store().audio_mb() >= st.record_cap_mb {
            if !st.cap_warned {
                st.cap_warned = true;
                self.log(
                    "warn",
                    &format!(
                        "Recording paused: saved audio has reached the {:.0} MB limit (recording.max_total_mb). Delete old conversations to free space.",
                        st.record_cap_mb
                    ),
                    None,
                );
            }
            return None;
        }
        let index = st.conv.as_ref().filter(|c| c["id"] == conv_id).and_then(|c| c["messages"].as_array()).map(|m| m.len())?;
        match self.store().save_audio(conv_id, index, role, pcm, rate) {
            Ok(name) => name,
            Err(e) => {
                self.log("warn", &format!("Could not save the recording: {}", e), None);
                None
            }
        }
    }

    async fn stt(self: &Arc<Self>, turn: &TurnRef, pcm: &[u8], early: Option<JoinHandle<Result<SttResult, ProviderError>>>) -> Result<(), ProviderError> {
        let stage = self.st.lock().unwrap().stages["stt"].clone();
        self.send(json!({"type": "status", "state": "transcribing"}));
        let t = Instant::now();
        let was_early = early.is_some();
        let res = match early {
            Some(handle) => handle.await.unwrap_or_else(|_| Err(providers::error(&stage, "the transcription was stopped"))),
            None => self.transcribe_task(pcm.to_vec()).await.unwrap_or_else(|_| Err(providers::error(&stage, "the transcription was stopped"))),
        }?;
        let waited = t.elapsed().as_millis() as u64;
        {
            let mut tn = turn.lock().unwrap();
            tn.latency_ms.insert("stt_wait".into(), json!(waited));
            tn.user_text = res.text.clone();
            tn.stt_seconds = (res.seconds * 1000.0).round() / 1000.0;
        }
        self.raw("stt", res.raw.clone());
        let (cost, source, extra) = {
            let mut st = self.st.lock().unwrap();
            let rates = st.cfg.rates(&stage);
            let (cost, source) = pricing::stt_cost(rates.as_ref(), res.seconds, res.api_cost);
            (cost, source, std::mem::take(&mut st.orphan_stt_cost))
        };
        self.log(
            "info",
            &format!(
                "STT {}: {:.1}s audio, waited {} ms{}, ${:.6} ({}){}",
                stage.model,
                res.seconds,
                waited,
                if was_early { " (started early during the pause)" } else { "" },
                cost,
                source,
                if extra > 0.0 { format!(" + ${:.6} for discarded early transcriptions", extra) } else { String::new() }
            ),
            None,
        );
        self.add_cost(turn, "stt", cost + extra, &source);
        Ok(())
    }

    fn first_audio(&self, turn: &TurnRef) {
        let min_silence = self.st.lock().unwrap().min_silence_ms;
        let index = {
            let mut t = turn.lock().unwrap();
            if t.latency_ms.contains_key("first_audio") {
                return;
            }
            let ms = t.t0.map(|t0| t0.elapsed().as_millis() as u64).unwrap_or(0);
            t.latency_ms.insert("first_audio".into(), json!(ms));
            if t.input != "greeting" {
                // For voice, add the silence the VAD waited before deciding the user had finished,
                // so the number is "you stopped talking -> you hear the reply".
                let extra = if t.input == "voice" { min_silence as u64 } else { 0 };
                t.latency_ms.insert("response".into(), json!(ms + extra));
            }
            t.index
        };
        self.send(json!({"type": "audio_start", "turn": index, "sample_rate": TTS_SAMPLE_RATE}));
        self.send(json!({"type": "status", "state": "speaking"}));
    }

    async fn reply(self: &Arc<Self>, turn: &TurnRef, greeting: Option<String>) -> Result<(), ProviderError> {
        let (tx, rx) = unbounded_channel::<Option<String>>();
        let speak = self.tts_pipeline(turn, rx);
        let write = async {
            match greeting {
                Some(g) => {
                    let index = {
                        let mut t = turn.lock().unwrap();
                        t.assistant_text = g.clone();
                        t.index
                    };
                    self.send(json!({"type": "transcript", "role": "assistant", "turn": index, "text": g, "final": false}));
                    let _ = tx.send(Some(g));
                }
                None => self.llm(turn, &tx).await?,
            }
            let _ = tx.send(None);
            Ok::<(), ProviderError>(())
        };
        tokio::try_join!(write, speak)?;
        Ok(())
    }

    fn system_prompt(&self, st: &State) -> String {
        let mut parts = vec![st.mode(self.store()).get("system_prompt").and_then(|v| v.as_str()).unwrap_or("").trim().to_string()];
        let mine = self.store().instructions();
        let here = st.conv.as_ref().map(|c| text_of(c, "instructions")).unwrap_or_default();
        if !mine.trim().is_empty() {
            parts.push(format!("The user's standing instructions for every conversation (follow them):\n{}", mine.trim()));
        }
        if !here.trim().is_empty() {
            parts.push(format!("The user's instructions for this conversation (follow them; they win over anything above):\n{}", here.trim()));
        }
        parts.join("\n\n")
    }

    async fn llm(self: &Arc<Self>, turn: &TurnRef, sentences: &UnboundedSender<Option<String>>) -> Result<(), ProviderError> {
        let (stage, messages) = {
            let st = self.st.lock().unwrap();
            let max_msgs = st.cfg.f64(&["assistant", "max_history_turns"], 20.0) as usize * 2 + 1;
            let mut messages = vec![json!({"role": "system", "content": self.system_prompt(&st)})];
            if let Some(list) = st.conv.as_ref().and_then(|c| c["messages"].as_array()) {
                for m in &list[list.len().saturating_sub(max_msgs)..] {
                    messages.push(json!({"role": m["role"], "content": m["content"]}));
                }
            }
            (st.stages["llm"].clone(), Value::Array(messages))
        };
        let index = {
            let mut t = turn.lock().unwrap();
            t.llm_prompt_chars = messages.as_array().unwrap().iter().map(|m| m["content"].as_str().map(|s| s.len()).unwrap_or(0)).sum();
            t.index
        };
        self.send(json!({"type": "status", "state": "thinking"}));
        let started = Instant::now();
        let (mut pending, mut first, mut sent_any) = (String::new(), true, false);
        let result = providers::chat_stream(&self.sh.http, &stage, &messages, None, |delta| {
            let text = {
                let mut t = turn.lock().unwrap();
                if first {
                    first = false;
                    t.latency_ms.insert("llm_first_token".into(), json!(started.elapsed().as_millis() as u64));
                }
                t.assistant_text.push_str(delta);
                t.assistant_text.clone()
            };
            self.send(json!({"type": "transcript", "role": "assistant", "turn": index, "text": text, "final": false}));
            pending.push_str(delta);
            // Hand complete sentences to TTS as they arrive.
            for s in take_sentences(&mut pending, &mut sent_any) {
                let _ = sentences.send(Some(s));
            }
        })
        .await;
        let has_text = {
            let mut t = turn.lock().unwrap();
            t.latency_ms.insert("llm".into(), json!(started.elapsed().as_millis() as u64));
            !t.assistant_text.trim().is_empty()
        };
        match result {
            Ok((usage, raw)) => {
                if let Some(r) = raw {
                    self.raw("llm", r);
                }
                if !pending.trim().is_empty() {
                    let _ = sentences.send(Some(pending.trim().to_string()));
                }
                if usage.is_some() || has_text {
                    self.bill_llm(turn, usage);
                }
                if !has_text {
                    return Err(providers::error(&stage, "the model returned an empty reply"));
                }
                Ok(())
            }
            Err(e) => {
                // Bill what was generated before the failure, but not a call that produced nothing.
                if has_text {
                    self.bill_llm(turn, None);
                }
                Err(e)
            }
        }
    }

    fn bill_llm(&self, turn: &TurnRef, usage: Option<Value>) {
        let (stage, rates) = {
            let st = self.st.lock().unwrap();
            let s = st.stages["llm"].clone();
            let r = st.cfg.rates(&s);
            (s, r)
        };
        let (cost, source, line) = {
            let mut t = turn.lock().unwrap();
            if t.llm_billed {
                return;
            }
            t.llm_billed = true;
            let estimated = usage.is_none();
            // provider sent no usage block: rough estimate, ~4 characters per token
            let usage = usage.unwrap_or_else(|| json!({"prompt_tokens": t.llm_prompt_chars / 4, "completion_tokens": t.assistant_text.len() / 4}));
            t.llm_input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            t.llm_output_tokens = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let (cost, mut source) =
                pricing::llm_cost(rates.as_ref(), t.llm_input_tokens, t.llm_output_tokens, usage.get("cost").and_then(|c| c.as_f64()));
            if stage.custom {
                source = source.replace("pricing.yaml", "your price");
            }
            if estimated {
                source.push_str(" (tokens estimated)");
            }
            let first = t.latency_ms.get("llm_first_token").map(|v| v.to_string()).unwrap_or_else(|| "?".into());
            let done = t.latency_ms.get("llm").map(|v| v.to_string()).unwrap_or_else(|| "?".into());
            let line = format!(
                "LLM {}: {} in / {} out tokens, first token {} ms, done {} ms, ${:.6} ({})",
                stage.model, t.llm_input_tokens, t.llm_output_tokens, first, done, cost, source
            );
            (cost, source, line)
        };
        self.log("info", &line, None);
        self.add_cost(turn, "llm", cost, &source);
    }

    /// Synthesises up to TTS_PARALLEL sentences at once and forwards the audio in order.
    async fn tts_pipeline(self: &Arc<Self>, turn: &TurnRef, mut sentences: UnboundedReceiver<Option<String>>) -> Result<(), ProviderError> {
        let (stage, voice, speed, rates) = {
            let st = self.st.lock().unwrap();
            let s = st.stages["tts"].clone();
            let speed = st.user_speed.or_else(|| st.mode(self.store()).get("tts_speed").and_then(|v| v.as_f64()));
            let r = st.cfg.rates(&s);
            (s, st.user_voice.clone(), speed, r)
        };
        let limit = Arc::new(Semaphore::new(TTS_PARALLEL));
        let mut jobs: VecDeque<UnboundedReceiver<TtsItem>> = VecDeque::new();
        let mut tasks = JoinSet::new(); // dropping it stops every synthesis still running
        let mut more = true;
        loop {
            tokio::select! {
                next = sentences.recv(), if more => match next {
                    Some(Some(text)) => {
                        let (tx, rx) = unbounded_channel();
                        jobs.push_back(rx);
                        let (me, turn, stage, voice, rates, limit) = (self.clone(), turn.clone(), stage.clone(), voice.clone(), rates.clone(), limit.clone());
                        tasks.spawn(async move {
                            let _permit = limit.acquire_owned().await;
                            let t = Instant::now();
                            // Billed for the characters submitted, so count the cost up front.
                            let chars = text.chars().count();
                            let (cost, source) = pricing::tts_cost(rates.as_ref(), chars);
                            turn.lock().unwrap().tts_characters += chars;
                            me.add_cost(&turn, "tts", cost, &source);
                            let mut first = None;
                            let result = providers::synthesize(&me.sh.http, &stage, &text, voice, speed, |audio| {
                                first.get_or_insert_with(|| t.elapsed().as_millis());
                                let _ = tx.send(TtsItem::Audio(audio));
                            }).await;
                            match result {
                                Ok(()) => {
                                    let source = if stage.custom { source.replace("pricing.yaml", "your price") } else { source };
                                    me.log("info", &format!("TTS {}: {} chars, first audio {} ms, done {} ms, ${:.6} ({})",
                                        stage.model, chars, first.map(|f| f.to_string()).unwrap_or_else(|| "?".into()), t.elapsed().as_millis(), cost, source), None);
                                    let _ = tx.send(TtsItem::Done);
                                }
                                Err(e) => { let _ = tx.send(TtsItem::Failed(e)); }
                            }
                        });
                    }
                    _ => more = false,
                },
                item = async { jobs.front_mut().unwrap().recv().await }, if !jobs.is_empty() => match item {
                    Some(TtsItem::Audio(audio)) => {
                        self.first_audio(turn);
                        turn.lock().unwrap().audio.extend_from_slice(&audio);
                        let _ = self.out.send(Out::Audio(audio));
                    }
                    Some(TtsItem::Failed(e)) => return Err(e),
                    Some(TtsItem::Done) | None => { jobs.pop_front(); }
                },
                else => break,
            }
        }
        Ok(())
    }

    fn finalize(&self, turn: &TurnRef, outcome: Outcome, t_start: Instant) {
        // A reply cut off mid-stream still cost tokens: bill what was generated.
        let late_bill = {
            let t = turn.lock().unwrap();
            !t.llm_billed && t.input != "greeting" && !t.assistant_text.is_empty()
        };
        if late_bill {
            turn.lock().unwrap().latency_ms.entry("llm").or_insert(json!(t_start.elapsed().as_millis() as u64));
            self.bill_llm(turn, None);
        }
        match outcome {
            Outcome::Done => {}
            Outcome::Cancelled => turn.lock().unwrap().interrupted = true,
            Outcome::Failed(e) => {
                turn.lock().unwrap().errors.push(e.message.clone());
                self.log("error", &e.message, (!e.raw.is_empty()).then(|| json!({"raw": e.raw})));
                self.send(json!({"type": "error", "message": e.message}));
            }
        }
        let mut out = Vec::new();
        let (line, record) = {
            let mut st = self.st.lock().unwrap();
            let mut t = turn.lock().unwrap();
            t.t0 = None;
            t.latency_ms.insert("total".into(), json!(t_start.elapsed().as_millis() as u64));
            let reply_audio = std::mem::take(&mut t.audio);
            let reply = t.assistant_text.trim().to_string();
            let response = t.latency_ms.get("response").cloned().unwrap_or(Value::Null);
            let audio_name = if reply.is_empty() { None } else { self.save_audio(&mut st, &t.conversation_id, "assistant", &reply_audio, TTS_SAMPLE_RATE) };
            let mut spoken = 0.0;
            let mut save_error = None;
            if let Some(conv) = st.conv.as_mut().filter(|c| c["id"] == t.conversation_id.as_str()) {
                if !reply.is_empty() {
                    let mut msg = json!({"role": "assistant", "content": reply, "ts": now_iso(), "cost": t.turn_total, "response_ms": response});
                    if t.interrupted {
                        msg["interrupted"] = json!(true);
                    }
                    if let Some(a) = &audio_name {
                        msg["audio"] = json!(a);
                    }
                    if let Some(list) = conv["messages"].as_array_mut() {
                        list.push(msg);
                    }
                }
                if t.user_saved || !reply.is_empty() {
                    conv["turns"] = json!(conv.get("turns").and_then(|v| v.as_u64()).unwrap_or(0) + t.user_saved as u64);
                    if let Err(e) = self.store().save(conv) {
                        save_error = Some(e.to_string());
                    }
                    out.push(json!({"type": "conversations", "items": self.store().summaries(), "folders": self.store().folders()}));
                }
                spoken = (conv.get("user_seconds").and_then(|v| v.as_f64()).unwrap_or(0.0) * 10.0).round() / 10.0;
            }
            if let Some(e) = save_error {
                self.log("error", &format!("Could not save the conversation to disk: {}", e), None);
            }
            out.push(json!({"type": "transcript", "role": "assistant", "turn": t.index, "text": t.assistant_text, "final": true,
                "interrupted": t.interrupted, "cost": t.turn_total, "response_ms": response, "audio": audio_name, "user_seconds": spoken}));
            out.push(json!({"type": "audio_end", "turn": t.index}));
            let line = format!(
                "Turn {} done: llm={} in={} out={} | llm ${:.6} + stt ${:.6} + tts ${:.6} = ${:.6} | session ${:.6}{}{}",
                t.index,
                st.stages["llm"].model,
                t.llm_input_tokens,
                t.llm_output_tokens,
                t.llm_cost,
                t.stt_cost,
                t.tts_cost,
                t.turn_total,
                st.totals["total"],
                if response.is_null() { String::new() } else { format!(" | first audio after {} ms", response) },
                if t.interrupted { " (interrupted)" } else { "" }
            );
            let record = serde_json::to_value(&*t).unwrap_or(Value::Null);
            st.turns.push(record.clone());
            out.push(st.cost_msg(Some(&t)));
            (line, record)
        };
        let mut msgs = out.into_iter();
        // conversations list first (if any), then the final transcript and the rest, then the log line
        for m in msgs.by_ref() {
            self.send(m);
        }
        self.log("info", &line, Some(json!({"turn": record})));
        self.send(json!({"type": "status", "state": "listening"}));
        self.save_log();
    }

    // ---------- per-connection debug log ----------

    fn save_log(&self) {
        let doc = {
            let st = self.st.lock().unwrap();
            json!({
                "session_id": self.id,
                "start_time": self.start_time,
                "updated_time": now_iso(),
                "preset": st.preset,
                "models": st.models(),
                "turns": st.turns,
                "cost_breakdown": {"llm": round8(st.totals["llm"]), "stt": round8(st.totals["stt"]), "tts": round8(st.totals["tts"])},
                "session_total_cost": round8(st.totals["total"]),
                "events": *self.events.lock().unwrap(),
            })
        };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.log_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.log_path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&doc).unwrap_or_default())?;
            std::fs::rename(&tmp, &self.log_path)
        };
        if let Err(e) = write() {
            crate::log("error", &format!("Could not write session log {}: {}", self.log_path.display(), e));
        }
    }

    // ---------- feedback report ----------

    pub fn start_review(self: &Arc<Self>) {
        let mut st = self.st.lock().unwrap();
        if st.review.as_ref().map(|r| !r.is_finished()).unwrap_or(false) {
            return;
        }
        let me = self.clone();
        st.review = Some(tokio::spawn(async move { me.review().await }));
    }

    async fn review(self: &Arc<Self>) {
        let prepared = {
            let st = self.st.lock().unwrap();
            let prompt = st.mode(self.store()).get("review_prompt").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            match &st.conv {
                Some(conv) if !prompt.is_empty() => {
                    let list = conv["messages"].as_array().cloned().unwrap_or_default();
                    let said = list.iter().filter(|m| m["role"] == "user").count();
                    if said < 2 {
                        Err("Say at least two things first, then ask for feedback.")
                    } else {
                        let lines: Vec<String> = list[list.len().saturating_sub(80)..]
                            .iter()
                            .map(|m| format!("{}: {}", if m["role"] == "user" { "Learner" } else { "Partner" }, m["content"].as_str().unwrap_or("")))
                            .collect();
                        let stage = st.stages["llm"].clone();
                        let rates = st.cfg.rates(&stage);
                        Ok((text_of(conv, "id"), prompt, lines.join("\n"), said, stage, rates))
                    }
                }
                _ => Err("This mode has no feedback report."),
            }
        };
        let (conv_id, prompt, transcript, said, stage, rates) = match prepared {
            Ok(p) => p,
            Err(e) => {
                self.send(json!({"type": "review", "error": e}));
                return;
            }
        };
        self.send(json!({"type": "review", "working": true}));
        let messages = json!([{"role": "system", "content": prompt}, {"role": "user", "content": format!("Transcript:\n{}", transcript)}]);
        let t = Instant::now();
        let mut text = String::new();
        let usage = match providers::chat_stream(&self.sh.http, &stage, &messages, Some(1200), |d| text.push_str(d)).await {
            Ok((usage, _)) => usage,
            Err(e) => {
                self.log("error", &e.message, None);
                self.send(json!({"type": "review", "error": e.message}));
                return;
            }
        };
        let u = usage.unwrap_or_else(|| json!({"prompt_tokens": (prompt.len() + transcript.len()) / 4, "completion_tokens": text.len() / 4}));
        let (tin, tout) = (u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0), u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0));
        let (cost, source) = pricing::llm_cost(rates.as_ref(), tin, tout, u.get("cost").and_then(|c| c.as_f64()));
        self.log("info", &format!("Feedback report: {} in / {} out tokens in {} ms, ${:.6} ({})", tin, tout, t.elapsed().as_millis(), cost, source), None);
        let mut review = json!({"ts": now_iso(), "messages_reviewed": said, "cost": round8(cost)});
        let body = text.trim();
        let parsed = match (body.find('{'), body.rfind('}')) {
            (Some(a), Some(b)) if b > a => serde_json::from_str::<Value>(&body[a..=b]).ok().filter(|v| v.is_object()),
            _ => None,
        };
        match parsed {
            Some(data) => {
                for k in ["summary", "level", "strengths", "mistakes", "tips"] {
                    review[k] = data.get(k).cloned().unwrap_or(Value::Null);
                }
            }
            None => review["text"] = json!(body), // the model did not return JSON: show what it wrote
        }
        let (cost_msg, save_error) = {
            let mut st = self.st.lock().unwrap();
            *st.totals.get_mut("llm").unwrap() += cost;
            *st.totals.get_mut("total").unwrap() += cost;
            let mut save_error = None;
            match st.conv.as_mut().filter(|c| c["id"] == conv_id.as_str()) {
                Some(conv) => {
                    for k in ["llm", "total"] {
                        conv["totals"][k] = json!(conv["totals"][k].as_f64().unwrap_or(0.0) + cost);
                    }
                    conv["review"] = review.clone();
                    if let Err(e) = self.store().save(conv) {
                        save_error = Some(e.to_string());
                    }
                }
                None => {
                    // the user moved to another conversation meanwhile: save it to the one reviewed
                    if let Some(mut conv) = self.store().load(&conv_id) {
                        for k in ["llm", "total"] {
                            conv["totals"][k] = json!(conv["totals"][k].as_f64().unwrap_or(0.0) + cost);
                        }
                        conv["review"] = review.clone();
                        if let Err(e) = self.store().save(&mut conv) {
                            save_error = Some(e.to_string());
                        }
                    }
                }
            }
            (st.cost_msg(None), save_error)
        };
        if let Some(e) = save_error {
            self.log("error", &format!("Could not save the conversation to disk: {}", e), None);
        }
        self.send(json!({"type": "review", "review": review}));
        self.send(cost_msg);
    }

    /// Text typed in the UI.
    pub fn text_turn(self: &Arc<Self>, text: String) {
        self.warm();
        self.start_turn(TurnInput { pcm: None, text: Some(text), greeting: None, stt: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_are_split_as_they_complete() {
        let (mut pending, mut sent) = (String::new(), false);
        pending.push_str("Hi! Octopuses have three hearts. And two of them");
        let out = take_sentences(&mut pending, &mut sent);
        assert_eq!(out, vec!["Hi! Octopuses have three hearts."]); // the short "Hi!" is merged
        assert_eq!(pending, "And two of them");
        assert!(sent);
    }

    #[test]
    fn closing_quote_stays_with_its_sentence() {
        let (mut pending, mut sent) = ("You can say: \"I went to the market.\" What did you buy".to_string(), false);
        let out = take_sentences(&mut pending, &mut sent);
        assert_eq!(out, vec!["You can say: \"I went to the market.\""]);
        assert_eq!(pending, "What did you buy");
    }

    #[test]
    fn long_first_sentence_splits_at_a_clause_once() {
        let (mut pending, mut sent) = ("The sky looks blue because air molecules scatter sunlight, and they scatter the short blue".to_string(), false);
        let out = take_sentences(&mut pending, &mut sent);
        assert_eq!(out, vec!["The sky looks blue because air molecules scatter sunlight,"]);
        assert!(sent);
        pending.push_str(" waves the most, which is why we see blue, mostly, in every direction we look up");
        assert!(take_sentences(&mut pending, &mut sent).is_empty()); // only the first chunk is split early
    }

    #[test]
    fn helpers() {
        assert_eq!(squeeze("  a   b \n c ", 4), "a b ");
        assert_eq!(round8(0.123456789), 0.12345679);
        assert_eq!(text_of(&json!({"a": 5}), "a"), "5");
        assert_eq!(text_of(&json!({}), "a"), "");
    }
}
