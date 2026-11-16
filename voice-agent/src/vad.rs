//! Silero VAD (ONNX Runtime, CPU only) plus a small turn detector.
//!
//! Audio in: 16 kHz mono int16 PCM. The model runs on 512-sample windows (32 ms).
use ort::session::Session;
use ort::value::Tensor;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

pub const SAMPLE_RATE: usize = 16000;
const WINDOW: usize = 512;
const CONTEXT: usize = 64;
const WIN_MS: f64 = WINDOW as f64 * 1000.0 / SAMPLE_RATE as f64;

/// The shared model. One instance serves every connection; each call takes about 0.25 ms.
pub struct Vad {
    session: Mutex<Session>,
    _env: ort::environment::Environment,
}

impl Vad {
    pub fn load(model: &Path, runtime_lib: &Path) -> Result<Vad, String> {
        let env = ort::init_from(runtime_lib)
            .map_err(|e| format!("Could not load ONNX Runtime from {}: {}", runtime_lib.display(), e))?
            .build()
            .map_err(|e| format!("Could not start ONNX Runtime: {}", e))?;
        let session = Session::builder(&env)
            .map_err(|e| format!("Could not configure ONNX Runtime: {}", e))?
            .with_intra_threads(1)
            .map_err(|e| format!("Could not configure ONNX Runtime: {}", e))?
            .commit_from_file(model)
            .map_err(|e| format!("Could not load the VAD model {}: {}", model.display(), e))?;
        Ok(Vad { session: Mutex::new(session), _env: env })
    }

    /// Speech probability for one window. `state` and `context` are the caller's and are updated.
    fn prob(&self, window: &[i16], state: &mut Vec<f32>, context: &mut Vec<f32>) -> Result<f32, String> {
        let x: Vec<f32> = window.iter().map(|s| *s as f32 / 32768.0).collect();
        let mut input = context.clone();
        input.extend_from_slice(&x);
        *context = x[WINDOW - CONTEXT..].to_vec();
        let e = |e: ort::Error| e.to_string();
        let input = Tensor::from_array(([1usize, CONTEXT + WINDOW], input)).map_err(e)?;
        let st = Tensor::from_array(([2usize, 1, 128], state.clone())).map_err(e)?;
        let sr = Tensor::from_array(([0usize; 0], vec![SAMPLE_RATE as i64])).map_err(e)?;
        let mut sess = self.session.lock().unwrap();
        let out = sess.run(ort::inputs!["input" => input, "state" => st, "sr" => sr]).map_err(e)?;
        let p = out["output"].try_extract_tensor::<f32>().map_err(e)?.1[0];
        *state = out["stateN"].try_extract_tensor::<f32>().map_err(e)?.1.to_vec();
        Ok(p)
    }
}

#[derive(Debug)]
pub enum VadEvent {
    /// Speech probability crossed the threshold.
    Start,
    /// It has lasted long enough to be real speech.
    Confirmed,
    /// A short silence: the audio so far, for early transcription.
    Pause(Vec<u8>),
    /// The speaker carried on after a pause.
    Resume,
    /// The burst was too short to count.
    Cancel,
    /// The turn is over: the whole utterance.
    End(Vec<u8>),
}

pub struct TurnDetector {
    threshold: f32,
    neg_threshold: f32,
    silence_windows: usize,
    min_speech_windows: usize,
    max_windows: usize,
    pause_windows: usize,
    pre_roll: VecDeque<Vec<i16>>,
    pre_roll_max: usize,
    buf: Vec<i16>,
    odd: Option<u8>,
    state: Vec<f32>,
    context: Vec<f32>,
    speaking: bool,
    paused: bool,
    speech: Vec<i16>,
    windows: usize,
    speech_windows: usize,
    silence: usize,
}

fn to_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

impl TurnDetector {
    pub fn new(threshold: f64, min_silence_ms: f64, min_speech_ms: f64, pre_roll_ms: f64, max_utterance_s: f64, pause_ms: f64) -> Self {
        let pause_windows = if pause_ms > 0.0 && pause_ms < min_silence_ms { (pause_ms / WIN_MS) as usize } else { 0 };
        let mut d = TurnDetector {
            threshold: 0.5,
            neg_threshold: 0.35,
            silence_windows: ((min_silence_ms / WIN_MS) as usize).max(1),
            min_speech_windows: ((min_speech_ms / WIN_MS) as usize).max(1),
            max_windows: (max_utterance_s * 1000.0 / WIN_MS) as usize,
            pause_windows,
            pre_roll: VecDeque::new(),
            pre_roll_max: ((pre_roll_ms / WIN_MS) as usize).max(1),
            buf: Vec::new(),
            odd: None,
            state: vec![0.0; 2 * 128],
            context: vec![0.0; CONTEXT],
            speaking: false,
            paused: false,
            speech: Vec::new(),
            windows: 0,
            speech_windows: 0,
            silence: 0,
        };
        d.set_threshold(threshold);
        d
    }

    pub fn set_threshold(&mut self, threshold: f64) {
        self.threshold = threshold as f32;
        self.neg_threshold = (threshold as f32 - 0.15).max(0.05);
    }

    pub fn set_min_silence(&mut self, ms: f64) {
        self.silence_windows = ((ms / WIN_MS) as usize).max(self.pause_windows + 1);
    }

    pub fn reset(&mut self) {
        self.state = vec![0.0; 2 * 128];
        self.context = vec![0.0; CONTEXT];
        self.speaking = false;
        self.paused = false;
        self.speech.clear();
        self.windows = 0;
        self.speech_windows = 0;
        self.silence = 0;
        self.pre_roll.clear();
    }

    pub fn feed(&mut self, vad: &Vad, pcm: &[u8]) -> Vec<VadEvent> {
        let mut events = Vec::new();
        // Frames may split a 16-bit sample: carry the odd byte over.
        let mut bytes: Vec<u8> = Vec::with_capacity(pcm.len() + 1);
        if let Some(b) = self.odd.take() {
            bytes.push(b);
        }
        bytes.extend_from_slice(pcm);
        if bytes.len() % 2 == 1 {
            self.odd = bytes.pop();
        }
        self.buf.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        let mut offset = 0;
        while self.buf.len() - offset >= WINDOW {
            let window: Vec<i16> = self.buf[offset..offset + WINDOW].to_vec();
            offset += WINDOW;
            let p = match vad.prob(&window, &mut self.state, &mut self.context) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !self.speaking {
                if p >= self.threshold {
                    self.speaking = true;
                    self.speech.clear();
                    for w in &self.pre_roll {
                        self.speech.extend_from_slice(w);
                    }
                    self.windows = self.pre_roll.len() + 1;
                    self.speech.extend_from_slice(&window);
                    self.speech_windows = 1;
                    self.silence = 0;
                    events.push(VadEvent::Start);
                    if self.speech_windows == self.min_speech_windows {
                        events.push(VadEvent::Confirmed);
                    }
                } else {
                    if self.pre_roll.len() == self.pre_roll_max {
                        self.pre_roll.pop_front();
                    }
                    self.pre_roll.push_back(window);
                }
                continue;
            }
            self.speech.extend_from_slice(&window);
            self.windows += 1;
            if p >= self.neg_threshold {
                self.speech_windows += 1;
                if self.paused {
                    self.paused = false;
                    events.push(VadEvent::Resume);
                }
                self.silence = 0;
                if self.speech_windows == self.min_speech_windows {
                    events.push(VadEvent::Confirmed);
                }
            } else {
                self.silence += 1;
                if self.pause_windows > 0
                    && self.silence == self.pause_windows
                    && self.speech_windows >= self.min_speech_windows
                    && self.windows < self.max_windows
                {
                    self.paused = true;
                    events.push(VadEvent::Pause(to_bytes(&self.speech)));
                }
            }
            if self.silence >= self.silence_windows || self.windows >= self.max_windows {
                let long_enough = self.speech_windows >= self.min_speech_windows;
                let audio = to_bytes(&self.speech);
                self.speaking = false;
                self.paused = false;
                self.speech.clear();
                self.pre_roll.clear();
                events.push(if long_enough { VadEvent::End(audio) } else { VadEvent::Cancel });
            }
        }
        self.buf.drain(..offset);
        events
    }
}
