//! Everything saved under data/: conversations, recordings, folders, modes and models made
//! in the UI, and the owner's instructions. One JSON file per conversation.
use crate::providers::{parse_wav, pcm_to_wav, Resampler, TTS_SAMPLE_RATE};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Conversation and folder ids: exactly 12 lowercase hex characters.
pub fn valid_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Ids of modes and models made in the UI: "custom-" + 8 hex characters.
pub fn valid_custom_id(id: &str) -> bool {
    id.strip_prefix("custom-").map(|h| h.len() == 8 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).unwrap_or(false)
}

/// Recording file names: NNNN-user.wav or NNNN-assistant.wav.
pub fn valid_audio_name(name: &str) -> bool {
    let Some((num, rest)) = name.split_once('-') else { return false };
    num.len() == 4 && num.bytes().all(|b| b.is_ascii_digit()) && (rest == "user.wav" || rest == "assistant.wav")
}

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

fn write_json(path: &Path, value: &Value, private: bool) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value).unwrap_or_default())?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    let _ = private;
    std::fs::rename(&tmp, path)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn summary(conv: &Value) -> Value {
    let messages = conv.get("messages").and_then(|m| m.as_array());
    json!({
        "id": conv["id"],
        "title": conv.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "mode": conv.get("mode").and_then(|v| v.as_str()).unwrap_or(""),
        "folder": conv.get("folder").filter(|f| f.is_string()).cloned().unwrap_or(Value::Null),
        "updated": conv.get("updated").and_then(|v| v.as_str()).unwrap_or(""),
        "turns": conv.get("turns").and_then(|v| v.as_u64()).unwrap_or(0),
        "recorded": messages.map(|m| m.iter().any(|x| x.get("audio").map(|a| a.is_string()).unwrap_or(false))).unwrap_or(false),
        "total": (conv["totals"]["total"].as_f64().unwrap_or(0.0) * 1e6).round() / 1e6,
    })
}

pub struct Store {
    dir: PathBuf,
    audio_dir: PathBuf,
    base: PathBuf,
    /// Summaries of every conversation, kept in memory so listing never reads the disk.
    index: Mutex<HashMap<String, Value>>,
    audio_bytes: AtomicI64,
    files: Mutex<()>,
}

impl Store {
    pub fn open(dir: PathBuf, audio_dir: PathBuf) -> std::io::Result<Store> {
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(&audio_dir)?;
        let base = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| dir.clone());
        let store = Store { dir, audio_dir, base, index: Mutex::new(HashMap::new()), audio_bytes: AtomicI64::new(0), files: Mutex::new(()) };
        let mut index = HashMap::new();
        for entry in std::fs::read_dir(&store.dir)?.flatten() {
            let path = entry.path();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if path.extension().and_then(|e| e.to_str()) == Some("json") && valid_id(&stem) {
                if let Some(conv) = store.load(&stem) {
                    index.insert(stem, summary(&conv));
                }
            }
        }
        *store.index.lock().unwrap() = index;
        let mut bytes = 0i64;
        if let Ok(folders) = std::fs::read_dir(&store.audio_dir) {
            for folder in folders.flatten() {
                bytes += dir_size(&folder.path());
            }
        }
        store.audio_bytes.store(bytes, Ordering::Relaxed);
        Ok(store)
    }

    fn path(&self, conv_id: &str) -> Option<PathBuf> {
        valid_id(conv_id).then(|| self.dir.join(format!("{}.json", conv_id)))
    }

    // ---------- conversations ----------

    pub fn create(&self, mode: &str) -> Value {
        let t = now();
        json!({"id": new_id(), "title": "New conversation", "mode": mode, "created": t, "updated": t, "turns": 0,
               "totals": {"llm": 0.0, "stt": 0.0, "tts": 0.0, "total": 0.0}, "messages": []})
    }

    pub fn load(&self, conv_id: &str) -> Option<Value> {
        let mut conv = read_json(&self.path(conv_id)?)?;
        let obj = conv.as_object_mut()?;
        if !obj.get("messages").map(|m| m.is_array()).unwrap_or(false) {
            obj.insert("messages".into(), json!([]));
        }
        let mut totals = json!({"llm": 0.0, "stt": 0.0, "tts": 0.0, "total": 0.0});
        if let Some(old) = obj.get("totals").and_then(|t| t.as_object()) {
            for (k, v) in old {
                if v.is_number() {
                    totals[k] = v.clone();
                }
            }
        }
        obj.insert("totals".into(), totals);
        if obj.get("id").and_then(|v| v.as_str()) != Some(conv_id) {
            obj.insert("id".into(), json!(conv_id));
        }
        Some(conv)
    }

    /// Saves and stamps "updated".
    pub fn save(&self, conv: &mut Value) -> std::io::Result<()> {
        conv["updated"] = json!(now());
        self.write(conv)
    }

    /// Saves without touching "updated" (moving between folders is not an edit).
    fn write(&self, conv: &Value) -> std::io::Result<()> {
        let id = conv["id"].as_str().unwrap_or("");
        let path = self.path(id).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid conversation id"))?;
        let _guard = self.files.lock().unwrap();
        write_json(&path, conv, false)?;
        self.index.lock().unwrap().insert(id.to_string(), summary(conv));
        Ok(())
    }

    pub fn delete(&self, conv_id: &str) -> bool {
        let Some(path) = self.path(conv_id) else { return false };
        if !path.exists() {
            return false;
        }
        let _ = std::fs::remove_file(&path);
        self.index.lock().unwrap().remove(conv_id);
        let folder = self.audio_dir.join(conv_id);
        if folder.exists() {
            self.audio_bytes.fetch_sub(dir_size(&folder), Ordering::Relaxed);
            let _ = std::fs::remove_dir_all(&folder);
        }
        true
    }

    /// Drops a conversation whose file has disappeared (deleted or restored outside the app).
    pub fn forget(&self, conv_id: &str) {
        if self.path(conv_id).map(|p| !p.exists()).unwrap_or(true) {
            self.index.lock().unwrap().remove(conv_id);
        }
    }

    /// Newest first.
    pub fn summaries(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self.index.lock().unwrap().values().cloned().collect();
        out.sort_by(|a, b| b["updated"].as_str().cmp(&a["updated"].as_str()).then(b["id"].as_str().cmp(&a["id"].as_str())));
        out
    }

    // ---------- recordings ----------

    pub fn audio_path(&self, conv_id: &str, name: &str) -> Option<PathBuf> {
        (valid_id(conv_id) && valid_audio_name(name)).then(|| self.audio_dir.join(conv_id).join(name))
    }

    /// Writes one message's audio as WAV and returns its file name.
    pub fn save_audio(&self, conv_id: &str, index: usize, role: &str, pcm: &[u8], sample_rate: u32) -> std::io::Result<Option<String>> {
        let name = format!("{:04}-{}.wav", index, role);
        let Some(path) = self.audio_path(conv_id, &name) else { return Ok(None) };
        if pcm.is_empty() {
            return Ok(None);
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, pcm_to_wav(pcm, sample_rate))?;
        self.audio_bytes.fetch_add(pcm.len() as i64 + 44, Ordering::Relaxed);
        Ok(Some(name))
    }

    pub fn audio_mb(&self) -> f64 {
        self.audio_bytes.load(Ordering::Relaxed).max(0) as f64 / 1_000_000.0
    }

    /// The whole conversation as one WAV, in order, with a short gap between messages.
    pub fn full_recording(&self, conv: &Value) -> Option<Vec<u8>> {
        let id = conv["id"].as_str()?;
        let gap = vec![0u8; (TTS_SAMPLE_RATE as f64 * 0.45) as usize * 2];
        let mut pcm: Vec<u8> = Vec::new();
        for m in conv["messages"].as_array()? {
            let Some(path) = m.get("audio").and_then(|a| a.as_str()).and_then(|a| self.audio_path(id, a)) else { continue };
            let Ok(data) = std::fs::read(&path) else { continue };
            let Ok((rate, _, samples)) = parse_wav(&data) else { continue };
            if rate == TTS_SAMPLE_RATE {
                pcm.extend_from_slice(samples);
            } else {
                pcm.extend_from_slice(&Resampler::new(rate).feed(samples));
            }
            pcm.extend_from_slice(&gap);
        }
        (!pcm.is_empty()).then(|| pcm_to_wav(&pcm, TTS_SAMPLE_RATE))
    }

    // ---------- folders ----------

    /// Ordered list of {"id", "name"}.
    pub fn folders(&self) -> Vec<Value> {
        read_json(&self.base.join("folders.json"))
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f.get("id").and_then(|i| i.as_str()).map(valid_id).unwrap_or(false) && f.get("name").and_then(|n| n.as_str()).map(|n| !n.is_empty()).unwrap_or(false))
            .collect()
    }

    fn save_folders(&self, folders: Vec<Value>) -> std::io::Result<()> {
        write_json(&self.base.join("folders.json"), &Value::Array(folders), false)
    }

    pub fn has_folder(&self, folder_id: &str) -> bool {
        self.folders().iter().any(|f| f["id"] == folder_id)
    }

    pub fn create_folder(&self, name: &str) -> std::io::Result<String> {
        let mut folders = self.folders();
        let id = new_id();
        folders.push(json!({"id": id, "name": name}));
        self.save_folders(folders)?;
        Ok(id)
    }

    pub fn rename_folder(&self, folder_id: &str, name: &str) -> std::io::Result<bool> {
        let mut folders = self.folders();
        let Some(f) = folders.iter_mut().find(|f| f["id"] == folder_id) else { return Ok(false) };
        f["name"] = json!(name);
        self.save_folders(folders)?;
        Ok(true)
    }

    /// Removes the folder; its conversations are kept and become unfiled.
    pub fn delete_folder(&self, folder_id: &str) -> std::io::Result<bool> {
        let folders = self.folders();
        let kept: Vec<Value> = folders.iter().filter(|f| f["id"] != folder_id).cloned().collect();
        if kept.len() == folders.len() {
            return Ok(false);
        }
        let inside: Vec<String> =
            self.index.lock().unwrap().values().filter(|s| s["folder"] == folder_id).filter_map(|s| s["id"].as_str().map(String::from)).collect();
        for id in inside {
            if let Some(mut conv) = self.load(&id) {
                conv["folder"] = Value::Null;
                self.write(&conv)?;
            }
        }
        self.save_folders(kept)?;
        Ok(true)
    }

    pub fn reorder_folders(&self, ids: &[Value]) -> std::io::Result<()> {
        let mut rest = self.folders();
        let mut ordered = Vec::new();
        for id in ids.iter().filter_map(|i| i.as_str()) {
            if let Some(pos) = rest.iter().position(|f| f["id"] == id) {
                ordered.push(rest.remove(pos));
            }
        }
        ordered.extend(rest);
        self.save_folders(ordered)
    }

    pub fn move_conversation(&self, conv_id: &str, folder_id: Option<&str>) -> std::io::Result<bool> {
        if let Some(f) = folder_id {
            if !self.has_folder(f) {
                return Ok(false);
            }
        }
        let Some(mut conv) = self.load(conv_id) else { return Ok(false) };
        conv["folder"] = folder_id.map(|f| json!(f)).unwrap_or(Value::Null);
        self.write(&conv)?;
        Ok(true)
    }

    // ---------- modes and models made in the UI, and the owner's instructions ----------

    fn map_file(&self, name: &str) -> Map<String, Value> {
        read_json(&self.base.join(name)).and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    pub fn custom_modes(&self) -> Map<String, Value> {
        self.map_file("custom_modes.json")
    }

    pub fn save_custom_mode(&self, name: &str, instructions: &str, greeting: &str) -> std::io::Result<String> {
        let mut modes = self.custom_modes();
        let id = format!("custom-{}", &new_id()[..8]);
        let short: String = instructions.chars().take(90).collect();
        let description = if instructions.chars().count() > 90 { format!("{}…", short) } else { short };
        modes.insert(id.clone(), json!({"name": name, "description": description, "instructions": instructions, "greeting": greeting, "custom": true}));
        write_json(&self.base.join("custom_modes.json"), &Value::Object(modes), false)?;
        Ok(id)
    }

    pub fn delete_custom_mode(&self, mode_id: &str) -> std::io::Result<bool> {
        let mut modes = self.custom_modes();
        if !valid_custom_id(mode_id) || modes.remove(mode_id).is_none() {
            return Ok(false);
        }
        write_json(&self.base.join("custom_modes.json"), &Value::Object(modes), false)?;
        Ok(true)
    }

    pub fn custom_models(&self) -> Map<String, Value> {
        self.map_file("custom_models.json")
    }

    /// The file can hold API keys, so it is written readable by the owner only.
    pub fn save_custom_model(&self, spec: Value) -> std::io::Result<String> {
        let mut models = self.custom_models();
        let id = format!("custom-{}", &new_id()[..8]);
        models.insert(id.clone(), spec);
        write_json(&self.base.join("custom_models.json"), &Value::Object(models), true)?;
        Ok(id)
    }

    pub fn delete_custom_model(&self, model_id: &str) -> std::io::Result<bool> {
        let mut models = self.custom_models();
        if !valid_custom_id(model_id) || models.remove(model_id).is_none() {
            return Ok(false);
        }
        write_json(&self.base.join("custom_models.json"), &Value::Object(models), true)?;
        Ok(true)
    }

    pub fn instructions(&self) -> String {
        self.map_file("profile.json").get("instructions").and_then(|v| v.as_str()).unwrap_or("").to_string()
    }

    pub fn save_instructions(&self, text: &str) -> std::io::Result<()> {
        let mut profile = self.map_file("profile.json");
        profile.insert("instructions".into(), json!(text));
        write_json(&self.base.join("profile.json"), &Value::Object(profile), false)
    }
}

fn dir_size(dir: &Path) -> i64 {
    std::fs::read_dir(dir).map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len() as i64).sum()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (PathBuf, Store) {
        let root = std::env::temp_dir().join(format!("voice-agent-test-{}", new_id()));
        let store = Store::open(root.join("conversations"), root.join("audio")).unwrap();
        (root, store)
    }

    #[test]
    fn conversation_roundtrip_list_delete() {
        let (root, store) = temp();
        let mut conv = store.create("english");
        conv["messages"] = json!([{"role": "user", "content": "hello"}]);
        conv["totals"]["total"] = json!(0.5);
        store.save(&mut conv).unwrap();
        let id = conv["id"].as_str().unwrap().to_string();
        assert_eq!(store.load(&id).unwrap()["messages"][0]["content"], "hello");
        assert_eq!(store.summaries()[0]["id"], id.as_str());
        // a second Store on the same folder rebuilds the index from disk
        let again = Store::open(root.join("conversations"), root.join("audio")).unwrap();
        assert_eq!(again.summaries().len(), 1);
        assert!(store.delete(&id));
        assert!(store.load(&id).is_none());
        assert!(store.summaries().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_path_tricks() {
        let (root, store) = temp();
        assert!(store.load("../../etc/passwd").is_none());
        assert!(!store.delete("../x"));
        assert!(store.audio_path("aaaaaaaaaaaa", "../secret.wav").is_none());
        assert!(store.audio_path("../aaaaaaaaaa", "0000-user.wav").is_none());
        assert!(store.audio_path("aaaaaaaaaaaa", "0000-user.wav").is_some());
        assert!(!valid_custom_id("general"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recording_saved_stitched_and_deleted() {
        let (root, store) = temp();
        let mut conv = store.create("general");
        let id = conv["id"].as_str().unwrap().to_string();
        let user = store.save_audio(&id, 0, "user", &vec![0u8; 16000 * 2], 16000).unwrap().unwrap();
        let reply = store.save_audio(&id, 1, "assistant", &vec![0u8; 24000 * 2], 24000).unwrap().unwrap();
        conv["messages"] = json!([{"role": "user", "content": "a", "audio": user}, {"role": "assistant", "content": "b", "audio": reply}]);
        store.save(&mut conv).unwrap();
        let wav = store.full_recording(&conv).unwrap();
        // 1 s + 1 s of audio plus two 0.45 s gaps at 24 kHz 16-bit, after the 44-byte header
        let expected = (24000.0 * 2.9) as i64 * 2;
        assert!(((wav.len() as i64 - 44) - expected).abs() <= 4, "{} vs {}", wav.len() - 44, expected);
        assert!(store.audio_mb() > 0.0);
        store.delete(&id);
        assert!(!store.audio_path(&id, &user).unwrap().exists());
        assert_eq!(store.audio_mb(), 0.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn folders() {
        let (root, store) = temp();
        let mut conv = store.create("general");
        store.save(&mut conv).unwrap();
        let id = conv["id"].as_str().unwrap().to_string();
        let work = store.create_folder("Work").unwrap();
        let fun = store.create_folder("Fun").unwrap();
        assert!(store.move_conversation(&id, Some(&work)).unwrap());
        assert!(!store.move_conversation(&id, Some("000000000000")).unwrap());
        assert_eq!(store.summaries()[0]["folder"], work.as_str());
        store.reorder_folders(&[json!(fun), json!(work)]).unwrap();
        assert_eq!(store.folders()[0]["name"], "Fun");
        assert!(store.delete_folder(&work).unwrap());
        assert!(store.summaries()[0]["folder"].is_null()); // the conversation is kept, unfiled
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn custom_modes_models_and_instructions() {
        let (root, store) = temp();
        let mode = store.save_custom_mode("Pirate", "Talk like a pirate at all times.", "").unwrap();
        assert!(store.custom_modes().contains_key(&mode));
        assert!(store.delete_custom_mode(&mode).unwrap());
        assert!(!store.delete_custom_mode("general").unwrap());
        let model = store.save_custom_model(json!({"kind": "llm", "model": "x", "base_url": "http://h/v1"})).unwrap();
        assert!(store.custom_models().contains_key(&model));
        assert!(store.delete_custom_model(&model).unwrap());
        assert_eq!(store.instructions(), "");
        store.save_instructions("Call me Sam.").unwrap();
        assert_eq!(store.instructions(), "Call me Sam.");
        let _ = std::fs::remove_dir_all(root);
    }
}
