//! Thin HTTP clients for the three model calls. OpenAI-compatible endpoints.
//!
//! Every function returns a ProviderError whose message is safe to show in the UI.
use crate::config::Stage;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Reply audio sent to the browser: 24 kHz mono s16le.
pub const TTS_SAMPLE_RATE: u32 = 24000;
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub message: String,
    pub raw: String,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn error(stage: &Stage, message: &str) -> ProviderError {
    ProviderError { message: format!("{} failed ({}): {}", stage.kind.to_uppercase(), stage.name(), message), raw: String::new() }
}

fn http_error(stage: &Stage, status: u16, body: &str) -> ProviderError {
    let mut msg: String = body.trim().chars().take(300).collect();
    if msg.is_empty() {
        msg = "empty response".into();
    }
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        match v.get("error") {
            Some(Value::Object(o)) => {
                if let Some(m) = o.get("message").and_then(|m| m.as_str()) {
                    msg = m.to_string();
                }
            }
            Some(Value::String(s)) => msg = s.clone(),
            _ => {}
        }
    }
    let hint = match status {
        401 => Some("check the API key"),
        402 => Some("the account is out of credits"),
        404 => Some("check the model name"),
        429 => Some("rate limited, try again shortly"),
        _ => None,
    };
    if let Some(h) = hint {
        msg = format!("{} ({})", msg, h);
    }
    ProviderError {
        message: format!("{} failed ({}): HTTP {}: {}", stage.kind.to_uppercase(), stage.name(), status, msg),
        raw: body.chars().take(2000).collect(),
    }
}

fn network_error(stage: &Stage, e: &reqwest::Error) -> ProviderError {
    let msg = if e.is_connect() {
        format!(
            "could not connect to {}. Check the address, that the model server is running, and that this server can reach it",
            stage.base_url
        )
    } else if e.is_timeout() {
        format!("{} did not answer in time", stage.base_url)
    } else {
        format!("network error: {}", e)
    };
    error(stage, &msg)
}

fn request(http: &reqwest::Client, stage: &Stage, path: &str) -> Result<reqwest::RequestBuilder, ProviderError> {
    let key = stage.api_key().map_err(|m| ProviderError { message: m, raw: String::new() })?;
    let mut req = http.post(format!("{}{}", stage.base_url, path)).timeout(CALL_TIMEOUT);
    if !key.is_empty() {
        req = req.bearer_auth(key); // a local server may need no key
    }
    Ok(req)
}

/// Opens (or keeps open) a connection to each provider so the next real call is fast.
pub async fn warm(http: reqwest::Client, urls: Vec<String>, warmed: &Mutex<HashMap<String, Instant>>) {
    for url in urls {
        {
            let mut w = warmed.lock().unwrap();
            if w.get(&url).map(|t| t.elapsed() < Duration::from_secs(60)).unwrap_or(false) {
                continue;
            }
            w.insert(url.clone(), Instant::now());
        }
        let _ = http.head(&url).timeout(Duration::from_secs(5)).send().await;
    }
}

pub fn pcm_to_wav(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// Reads a 16-bit PCM WAV: (sample rate, channels, sample bytes).
pub fn parse_wav(data: &[u8]) -> Result<(u32, u16, &[u8]), String> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut pos, mut fmt) = (12usize, None);
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= data.len() {
            let format = u16::from_le_bytes([data[body], data[body + 1]]);
            let channels = u16::from_le_bytes([data[body + 2], data[body + 3]]);
            let rate = u32::from_le_bytes([data[body + 4], data[body + 5], data[body + 6], data[body + 7]]);
            let bits = u16::from_le_bytes([data[body + 14], data[body + 15]]);
            if (format != 1 && format != 0xFFFE) || bits != 16 {
                return Err(format!("{}-bit audio (format {}); only 16-bit PCM WAV is supported", bits, format));
            }
            fmt = Some((rate, channels.max(1)));
        } else if id == b"data" {
            let (rate, channels) = fmt.ok_or("WAV has no format chunk")?;
            // Streaming servers often write a placeholder size: take what is actually there.
            let end = if size == 0 || body + size > data.len() { data.len() } else { body + size };
            return Ok((rate, channels, &data[body..end]));
        }
        pos = body + size + (size & 1);
    }
    Err("WAV has no audio data".into())
}

/// Streaming linear resampler to TTS_SAMPLE_RATE, for endpoints that answer at another rate.
pub struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
    primed: bool,
}

impl Resampler {
    pub fn new(rate: u32) -> Self {
        Resampler { step: rate as f64 / TTS_SAMPLE_RATE as f64, pos: 0.0, last: 0.0, primed: false }
    }

    pub fn feed(&mut self, pcm: &[u8]) -> Vec<u8> {
        // x[0] is the last sample of the previous chunk, so interpolation is continuous.
        let mut x: Vec<f32> = Vec::with_capacity(pcm.len() / 2 + 1);
        x.push(self.last);
        x.extend(pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32));
        if !self.primed {
            self.primed = true;
            self.pos = 1.0; // start at the first real sample
        }
        let limit = (x.len() - 1) as f64;
        let mut out = Vec::with_capacity((x.len() as f64 / self.step) as usize * 2 + 2);
        let mut p = self.pos;
        while p < limit {
            let i = p as usize;
            let frac = (p - i as f64) as f32;
            let v = x[i] + (x[i + 1] - x[i]) * frac;
            out.extend_from_slice(&(v.round().clamp(-32768.0, 32767.0) as i16).to_le_bytes());
            p += self.step;
        }
        self.pos = p - limit;
        self.last = *x.last().unwrap();
        out
    }
}

/// Any 16-bit WAV to 24 kHz mono PCM bytes.
pub fn wav_to_pcm24k(data: &[u8]) -> Result<Vec<u8>, String> {
    let (rate, channels, samples) = parse_wav(data)?;
    let mono: Vec<u8> = if channels > 1 {
        samples
            .chunks_exact(2 * channels as usize)
            .flat_map(|frame| {
                let sum: i32 = frame.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).sum();
                ((sum / channels as i32) as i16).to_le_bytes()
            })
            .collect()
    } else {
        samples[..samples.len() / 2 * 2].to_vec()
    };
    Ok(if rate == TTS_SAMPLE_RATE { mono } else { Resampler::new(rate).feed(&mono) })
}

pub struct SttResult {
    pub text: String,
    pub seconds: f64,
    pub api_cost: Option<f64>,
    pub raw: Value,
}

pub async fn transcribe(
    http: reqwest::Client,
    stage: Stage,
    pcm: Vec<u8>,
    sample_rate: u32,
    language: Option<String>,
) -> Result<SttResult, ProviderError> {
    let seconds = pcm.len() as f64 / 2.0 / sample_rate as f64;
    let file = reqwest::multipart::Part::bytes(pcm_to_wav(&pcm, sample_rate))
        .file_name("speech.wav")
        .mime_str("audio/wav")
        .map_err(|e| error(&stage, &e.to_string()))?;
    let mut form = reqwest::multipart::Form::new().text("model", stage.model.clone()).part("file", file);
    if let Some(lang) = language.or_else(|| stage.opt_str("language")) {
        form = form.text("language", lang);
    }
    let resp = request(&http, &stage, "/audio/transcriptions")?.multipart(form).send().await.map_err(|e| network_error(&stage, &e))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| network_error(&stage, &e))?;
    if status != 200 {
        return Err(http_error(&stage, status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| error(&stage, "it did not return JSON"))?;
    let usage = v.get("usage").cloned().unwrap_or(Value::Null);
    Ok(SttResult {
        text: v.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string(),
        seconds: usage.get("seconds").and_then(|s| s.as_f64()).filter(|s| *s > 0.0).unwrap_or(seconds),
        api_cost: usage.get("cost").and_then(|c| c.as_f64()),
        raw: v,
    })
}

/// Streams a chat completion. `on_delta` gets each piece of text as it arrives.
/// Returns (usage block if the provider sent one, the last raw chunk).
pub async fn chat_stream<F: FnMut(&str)>(
    http: &reqwest::Client,
    stage: &Stage,
    messages: &Value,
    max_tokens: Option<u64>,
    mut on_delta: F,
) -> Result<(Option<Value>, Option<Value>), ProviderError> {
    let mut body = json!({
        "model": stage.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": max_tokens.or_else(|| stage.options.get("max_tokens").and_then(|v| v.as_u64())).unwrap_or(400),
    });
    if let Some(t) = stage.options.get("temperature") {
        body["temperature"] = t.clone();
    }
    if let Some(extra) = stage.options.get("extra").and_then(|e| e.as_object()) {
        for (k, v) in extra {
            body[k] = v.clone();
        }
    }
    let resp = request(http, stage, "/chat/completions")?.json(&body).send().await.map_err(|e| network_error(stage, &e))?;
    let status = resp.status().as_u16();
    if status != 200 {
        let text = resp.text().await.unwrap_or_default();
        return Err(http_error(stage, status, &text));
    }
    let (mut usage, mut last) = (None, None);
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    'outer: while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| network_error(stage, &e))?);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(payload) = line.trim().strip_prefix("data:") else { continue };
            let payload = payload.trim();
            if payload == "[DONE]" {
                break 'outer;
            }
            let Ok(v) = serde_json::from_str::<Value>(payload) else { continue };
            if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                let m = err.get("message").and_then(|m| m.as_str()).map(String::from).unwrap_or_else(|| err.to_string());
                return Err(error(stage, &m));
            }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                usage = Some(u.clone());
            }
            if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
                for c in choices {
                    if let Some(text) = c.get("delta").and_then(|d| d.get("content")).and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            on_delta(text);
                        }
                    }
                }
            }
            last = Some(v);
        }
    }
    Ok((usage, last))
}

/// Streams speech for `text`. `on_audio` gets 24 kHz mono PCM as it arrives, whatever rate
/// the endpoint answers in. With the tts option `format: wav` the whole file is fetched and
/// converted, for servers that cannot stream raw PCM.
pub async fn synthesize<F: FnMut(Vec<u8>)>(
    http: &reqwest::Client,
    stage: &Stage,
    text: &str,
    voice: Option<String>,
    speed: Option<f64>,
    mut on_audio: F,
) -> Result<(), ProviderError> {
    let fmt = stage.opt_str("format").unwrap_or_else(|| "pcm".into());
    let mut body = json!({"model": stage.model, "input": text, "response_format": fmt});
    if let Some(v) = voice.or_else(|| stage.opt_str("voice")) {
        body["voice"] = json!(v);
    }
    if let Some(s) = speed.or_else(|| stage.opt_f64("speed")) {
        if (s - 1.0).abs() > 0.01 {
            body["speed"] = json!(s);
        }
    }
    let resp = request(http, stage, "/audio/speech")?.json(&body).send().await.map_err(|e| network_error(stage, &e))?;
    let status = resp.status().as_u16();
    if status != 200 {
        let text = resp.text().await.unwrap_or_default();
        return Err(http_error(stage, status, &text));
    }
    let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    if fmt == "wav" || ctype.contains("wav") {
        let data = resp.bytes().await.map_err(|e| network_error(stage, &e))?;
        let pcm = wav_to_pcm24k(&data).map_err(|e| error(stage, &format!("could not read the WAV audio it returned: {}", e)))?;
        for chunk in pcm.chunks(8192) {
            on_audio(chunk.to_vec());
        }
        return Ok(());
    }
    if ["json", "text", "mpeg", "mp3", "ogg"].iter().any(|t| ctype.contains(t)) {
        return Err(error(
            stage,
            &format!(
                "expected raw PCM audio but got '{}'. Set this model's format to wav, or pick a model that supports response_format=pcm.",
                ctype
            ),
        ));
    }
    let rate = ctype
        .split("rate=")
        .nth(1)
        .and_then(|r| r.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().ok())
        .or_else(|| stage.options.get("sample_rate").and_then(|v| v.as_u64()).map(|v| v as u32))
        .unwrap_or(TTS_SAMPLE_RATE);
    let mut resampler = if rate != TTS_SAMPLE_RATE { Some(Resampler::new(rate)) } else { None };
    let mut carry: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        carry.extend_from_slice(&chunk.map_err(|e| network_error(stage, &e))?);
        let cut = carry.len() / 2 * 2; // keep 16-bit samples whole
        if cut == 0 {
            continue;
        }
        let whole: Vec<u8> = carry.drain(..cut).collect();
        let out = match resampler.as_mut() {
            Some(r) => r.feed(&whole),
            None => whole,
        };
        if !out.is_empty() {
            on_audio(out);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_pitch_and_length() {
        // One second of a 440 Hz tone at 44.1 kHz, fed in uneven chunks.
        let src: Vec<u8> = (0..44100)
            .flat_map(|i| (((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / 44100.0).sin() * 20000.0) as i16).to_le_bytes())
            .collect();
        let mut r = Resampler::new(44100);
        let out: Vec<u8> = src.chunks(8190).flat_map(|c| r.feed(c)).collect();
        let samples: Vec<i16> = out.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert!((samples.len() as i64 - 24000).abs() <= 2, "length {}", samples.len());
        let crossings = samples.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        assert!((crossings as i64 - 440).abs() <= 1, "crossings {}", crossings);
    }

    #[test]
    fn wav_roundtrip_and_placeholder_size() {
        let pcm: Vec<u8> = (0..2000i16).flat_map(|i| i.to_le_bytes()).collect();
        let mut wav = pcm_to_wav(&pcm, 24000);
        assert_eq!(wav_to_pcm24k(&wav).unwrap(), pcm);
        wav[40..44].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // streaming servers do this
        assert_eq!(wav_to_pcm24k(&wav).unwrap(), pcm);
        assert!(wav_to_pcm24k(b"not audio at all").is_err());
    }
}
