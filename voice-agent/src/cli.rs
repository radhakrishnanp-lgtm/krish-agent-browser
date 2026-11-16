//! Command-line helpers: `--check` (free) and `--smoke [preset]` (one paid call per stage).
use crate::config::{Config, KINDS};
use crate::{pricing, providers};
use serde_json::json;
use std::path::Path;

/// Validates the config and makes sure every catalog model has a price. No network calls.
pub fn check(root: &Path) -> i32 {
    let cfg = match Config::load(root) {
        Ok(c) => c,
        Err(e) => {
            println!("FAIL  config: {}", e);
            return 1;
        }
    };
    println!("ok    config loads; presets: {}", cfg.preset_names().join(", "));
    let mut missing = Vec::new();
    for kind in KINDS {
        let ids = cfg.catalog_ids(kind);
        for id in &ids {
            match cfg.stage(kind, id) {
                Ok(stage) if cfg.rates(&stage).is_none() => missing.push(format!("{}/{} ({} / {})", kind, id, stage.provider, stage.model)),
                _ => {}
            }
        }
        println!("ok    {} catalog: {} models", kind, ids.len());
    }
    if !missing.is_empty() {
        println!("FAIL  no price in config/pricing.yaml for: {}", missing.join(", "));
        return 1;
    }
    println!("ok    every catalog model has a price");
    println!("ok    modes: {}", cfg.modes.keys().cloned().collect::<Vec<_>>().join(", "));
    0
}

/// Calls each stage of a preset once and prints the result and cost.
/// Order: TTS (makes audio) -> STT (transcribes that audio) -> LLM.
pub async fn smoke(root: &Path, preset: Option<String>) -> i32 {
    let cfg = match Config::load(root) {
        Ok(c) => c,
        Err(e) => {
            println!("FAIL  config: {}", e);
            return 1;
        }
    };
    let preset = preset.unwrap_or_else(|| cfg.active_preset.clone());
    let stages = match cfg.resolve(&preset) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL  {}", e);
            return 1;
        }
    };
    println!("Preset: {}", preset);
    let http = reqwest::Client::new();
    let mut failed = 0;

    let (tts, text) = (&stages["tts"], "Hello, this is a smoke test.");
    let mut pcm24: Vec<u8> = Vec::new();
    match providers::synthesize(&http, tts, text, None, None, |a| pcm24.extend_from_slice(&a)).await {
        Ok(()) => {
            let (cost, src) = pricing::tts_cost(cfg.rates(tts).as_ref(), text.chars().count());
            println!("PASS  TTS  {}: {:.1}s audio, ${:.6} ({})", tts.model, pcm24.len() as f64 / 2.0 / providers::TTS_SAMPLE_RATE as f64, cost, src);
        }
        Err(e) => {
            failed += 1;
            println!("FAIL  {}", e);
        }
    }

    let stt = &stages["stt"];
    if pcm24.is_empty() {
        println!("SKIP  STT  (no audio because TTS failed)");
    } else {
        // 24 kHz -> 16 kHz by linear interpolation
        let s: Vec<f32> = pcm24.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32).collect();
        let mut pcm16 = Vec::with_capacity(pcm24.len() * 2 / 3 + 2);
        let mut p = 0.0f64;
        while (p as usize) + 1 < s.len() {
            let i = p as usize;
            let v = s[i] + (s[i + 1] - s[i]) * (p - i as f64) as f32;
            pcm16.extend_from_slice(&(v as i16).to_le_bytes());
            p += 1.5;
        }
        match providers::transcribe(http.clone(), stt.clone(), pcm16, 16000, None).await {
            Ok(r) => {
                let (cost, src) = pricing::stt_cost(cfg.rates(stt).as_ref(), r.seconds, r.api_cost);
                println!("PASS  STT  {}: \"{}\", ${:.6} ({})", stt.model, r.text, cost, src);
            }
            Err(e) => {
                failed += 1;
                println!("FAIL  {}", e);
            }
        }
    }

    let llm = &stages["llm"];
    let mut reply = String::new();
    match providers::chat_stream(&http, llm, &json!([{"role": "user", "content": "Say hello in five words."}]), None, |d| reply.push_str(d)).await {
        Ok((usage, _)) => {
            let note = if usage.is_some() { "" } else { "  WARNING: provider sent no usage block; the app will estimate tokens" };
            let u = usage.unwrap_or_else(|| json!({}));
            let (tin, tout) = (u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0), u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0));
            let (cost, src) = pricing::llm_cost(cfg.rates(llm).as_ref(), tin, tout, u.get("cost").and_then(|c| c.as_f64()));
            println!("PASS  LLM  {}: \"{}\" in={} out={}, ${:.6} ({}){}", llm.model, reply.trim(), tin, tout, cost, src, note);
        }
        Err(e) => {
            failed += 1;
            println!("FAIL  {}", e);
        }
    }
    if failed > 0 { 1 } else { 0 }
}
