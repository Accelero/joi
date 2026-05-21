// Mic capture worklet (SPEC §7.1). Runs on the audio render thread: copies each input block to the
// main thread for downsampling/framing, and gates muted audio HERE so it never leaves the device
// (FR-6). Plain JS — not type-checked; the testable math lives in dsp.ts.
class MicCapture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.muted = false;
    this.port.onmessage = (event) => {
      if (event.data && event.data.type === "mute") this.muted = event.data.muted;
    };
  }

  process(inputs) {
    const channel = inputs[0] && inputs[0][0];
    if (channel && !this.muted) {
      // Copy: the input buffer is reused by the engine after process() returns.
      this.port.postMessage(channel.slice(0));
    }
    return true; // keep the processor alive
  }
}

registerProcessor("mic-capture", MicCapture);
