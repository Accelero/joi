/**
 * Mic capture pipeline (SPEC §7.1): getUserMedia → AudioWorklet → downsample to 16 kHz mono PCM16
 * → 20 ms frames → backend. The DSP is the tested code in {@link ./dsp}; this module is the
 * un-testable Web-Audio glue around it (verified by the manual exit demo, PLAN §5 M-4).
 */
import {
  downsample,
  floatToPcm16,
  FrameAccumulator,
  samplesPerFrame,
  TARGET_INPUT_RATE,
} from "./dsp";

/** 20 ms framing (SPEC §7.1): 320 samples at 16 kHz. */
const FRAME_MS = 20;

/** Controls a running mic capture. */
export interface MicHandle {
  /** Mute/unmute at the worklet — the primary gate, so muted audio never leaves the device (FR-6). */
  setMuted(muted: boolean): void;
  /** Tear down the worklet, source, and media stream. */
  stop(): Promise<void>;
}

/**
 * Begin capturing the mic, delivering each finished 16 kHz PCM16 frame to `onFrame`.
 *
 * `onFrame` is the transport sink (e.g. `audio.sendMicFrame`). Capture starts unmuted.
 */
export async function startMic(onFrame: (pcm: Int16Array) => void): Promise<MicHandle> {
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true },
  });
  const ctx = new AudioContext();
  await ctx.audioWorklet.addModule(new URL("./mic-worklet.js", import.meta.url));

  const source = ctx.createMediaStreamSource(stream);
  const node = new AudioWorkletNode(ctx, "mic-capture", {
    numberOfInputs: 1,
    numberOfOutputs: 0,
  });

  const accumulator = new FrameAccumulator(samplesPerFrame(TARGET_INPUT_RATE, FRAME_MS));
  node.port.onmessage = (event: MessageEvent<Float32Array>) => {
    const down = downsample(event.data, ctx.sampleRate, TARGET_INPUT_RATE);
    for (const frame of accumulator.push(floatToPcm16(down))) {
      onFrame(frame);
    }
  };
  source.connect(node);

  return {
    setMuted(muted: boolean) {
      node.port.postMessage({ type: "mute", muted });
    },
    async stop() {
      source.disconnect();
      node.disconnect();
      for (const track of stream.getTracks()) track.stop();
      await ctx.close();
    },
  };
}
