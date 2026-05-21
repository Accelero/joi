// PCM playback worklet (SPEC §7.2). Holds a queue of Int16 frames and feeds the output one sample
// at a time, emitting silence on underrun and clearing instantly on "flush" (barge-in, FR-2).
// Plain JS — not type-checked.
class PcmPlayer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.queue = [];
    this.offset = 0;
    this.port.onmessage = (event) => {
      const msg = event.data;
      if (msg.type === "flush") {
        this.queue = [];
        this.offset = 0;
      } else if (msg.type === "samples") {
        this.queue.push(msg.pcm);
      }
    };
  }

  process(_inputs, outputs) {
    const out = outputs[0][0];
    for (let i = 0; i < out.length; i++) {
      if (this.queue.length === 0) {
        out[i] = 0; // underrun → silence
        continue;
      }
      const head = this.queue[0];
      out[i] = head[this.offset++] / 0x8000; // PCM16 → float [-1, 1)
      if (this.offset >= head.length) {
        this.queue.shift();
        this.offset = 0;
      }
    }
    return true;
  }
}

registerProcessor("pcm-player", PcmPlayer);
