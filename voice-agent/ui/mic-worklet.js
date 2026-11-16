// Captures microphone audio, downsamples to 16 kHz mono and posts Int16 PCM chunks.
class MicProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ratio = sampleRate / 16000; // input samples per output sample
    this.pos = 0; this.acc = 0; this.n = 0;
    this.out = new Int16Array(512); this.len = 0;
  }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (!ch) return true;
    let peak = 0;
    for (let i = 0; i < ch.length; i++) {
      const s = ch[i];
      const a = s < 0 ? -s : s; if (a > peak) peak = a;
      this.acc += s; this.n++; this.pos++;
      if (this.pos >= this.ratio) { // averaging acts as a simple low-pass filter
        const v = Math.max(-1, Math.min(1, this.acc / this.n));
        this.out[this.len++] = v * 32767;
        this.pos -= this.ratio; this.acc = 0; this.n = 0;
        if (this.len === this.out.length) {
          this.port.postMessage({ pcm: this.out.buffer, peak }, [this.out.buffer]);
          this.out = new Int16Array(512); this.len = 0;
        }
      }
    }
    return true;
  }
}
registerProcessor('mic-processor', MicProcessor);
