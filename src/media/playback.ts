/**
 * Playback pipeline (SPEC §7.2): provider 24 kHz mono PCM16 → AudioWorklet ring buffer → device.
 * `flush()` drops buffered audio instantly for barge-in (FR-2). The buffering semantics mirror
 * {@link ./dsp}'s `JitterBuffer` (the tested reference); the ring lives in the worklet because
 * `process()` is realtime and cannot pull across the port synchronously.
 */
import { OUTPUT_RATE } from "./dsp";

/** Controls a running playback node. */
export interface PlaybackHandle {
  /** Enqueue a decoded 24 kHz PCM16 output frame. */
  enqueue(pcm: Int16Array): void;
  /** Drop everything buffered immediately (barge-in / interrupt). */
  flush(): void;
  /** Tear down the node and context. */
  stop(): Promise<void>;
}

/** Open the output device and start an (initially silent) PCM player at 24 kHz. */
export async function startPlayback(): Promise<PlaybackHandle> {
  const ctx = new AudioContext({ sampleRate: OUTPUT_RATE });
  await ctx.audioWorklet.addModule(new URL("./playback-worklet.js", import.meta.url));

  const node = new AudioWorkletNode(ctx, "pcm-player", {
    numberOfInputs: 0,
    numberOfOutputs: 1,
    outputChannelCount: [1],
  });
  node.connect(ctx.destination);

  return {
    enqueue(pcm: Int16Array) {
      // Transfer the buffer to the worklet to avoid a copy; the caller holds no further reference.
      node.port.postMessage({ type: "samples", pcm }, [pcm.buffer]);
    },
    flush() {
      node.port.postMessage({ type: "flush" });
    },
    async stop() {
      node.disconnect();
      await ctx.close();
    },
  };
}
