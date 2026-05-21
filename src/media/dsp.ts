/**
 * Pure DSP helpers for the media pipeline (SPEC §7, §8.2).
 *
 * Everything here is framework- and DOM-free so it is unit-testable in plain Node — the
 * AudioWorklet / getUserMedia / playback glue that *uses* these helpers is not (PLAN §5, M-4).
 * Keeping the math isolated is what makes the otherwise un-testable Web-Audio layer trustworthy.
 */

/** Mic sample rate sent to the provider (Hz). Matches `AudioFormat::INPUT` in joi-core. */
export const TARGET_INPUT_RATE = 16_000;
/** Audio-output sample rate received from the provider (Hz). Matches `AudioFormat::OUTPUT`. */
export const OUTPUT_RATE = 24_000;

/** Samples in a `frameMs`-millisecond frame at `rate` (e.g. 16 kHz × 20 ms = 320). */
export function samplesPerFrame(rate: number, frameMs: number): number {
  return Math.floor((rate * frameMs) / 1000);
}

/** Clamp float [-1, 1] samples to signed 16-bit PCM. */
export function floatToPcm16(input: Float32Array): Int16Array {
  const out = new Int16Array(input.length);
  for (let i = 0; i < input.length; i++) {
    const s = Math.max(-1, Math.min(1, input[i]));
    out[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
  }
  return out;
}

/**
 * Linear-interpolation downsample of mono float audio to `targetRate`.
 *
 * The mic path only ever downsamples (browser rates are ≥ 44.1 kHz); upsampling is a no-op copy.
 */
export function downsample(
  input: Float32Array,
  inputRate: number,
  targetRate: number = TARGET_INPUT_RATE,
): Float32Array {
  if (inputRate <= 0 || targetRate <= 0) {
    throw new RangeError("sample rates must be positive");
  }
  if (targetRate >= inputRate || input.length === 0) {
    return input.slice();
  }
  const ratio = inputRate / targetRate;
  const outLen = Math.floor(input.length / ratio);
  const out = new Float32Array(outLen);
  for (let i = 0; i < outLen; i++) {
    const pos = i * ratio;
    const i0 = Math.floor(pos);
    const i1 = Math.min(i0 + 1, input.length - 1);
    const frac = pos - i0;
    out[i] = input[i0] * (1 - frac) + input[i1] * frac;
  }
  return out;
}

/**
 * Accumulates PCM samples and yields fixed-size frames, buffering any remainder across pushes —
 * the real worklet behaviour, where `onaudioprocess` blocks rarely align to 20 ms boundaries.
 */
export class FrameAccumulator {
  private remainder = new Int16Array(0);

  constructor(private readonly frameSize: number) {
    if (frameSize <= 0) throw new RangeError("frameSize must be positive");
  }

  /** Push samples; return any newly completed frames (each exactly `frameSize`). */
  push(samples: Int16Array): Int16Array[] {
    const merged = new Int16Array(this.remainder.length + samples.length);
    merged.set(this.remainder, 0);
    merged.set(samples, this.remainder.length);

    const frames: Int16Array[] = [];
    let offset = 0;
    while (merged.length - offset >= this.frameSize) {
      frames.push(merged.slice(offset, offset + this.frameSize));
      offset += this.frameSize;
    }
    this.remainder = merged.slice(offset);
    return frames;
  }

  /** Samples currently held back, waiting for a full frame. */
  get bufferedSamples(): number {
    return this.remainder.length;
  }
}

/**
 * A simple playback jitter buffer (SPEC §7.2). Enqueue provider PCM; pull fixed blocks for the
 * output device, padding with silence on underrun. `flush()` clears it instantly for barge-in
 * (FR-2).
 */
export class JitterBuffer {
  private queue: Int16Array[] = [];
  private buffered = 0;

  /** Append a chunk of output PCM. */
  enqueue(chunk: Int16Array): void {
    if (chunk.length === 0) return;
    this.queue.push(chunk);
    this.buffered += chunk.length;
  }

  /** Samples currently buffered. */
  get bufferedSamples(): number {
    return this.buffered;
  }

  /** Buffered duration in ms at `rate`. */
  bufferedMs(rate: number = OUTPUT_RATE): number {
    return (this.buffered / rate) * 1000;
  }

  /** Pull exactly `n` samples; missing tail is silence (zeros) on underrun. */
  pull(n: number): Int16Array {
    const out = new Int16Array(n);
    let filled = 0;
    while (filled < n && this.queue.length > 0) {
      const head = this.queue[0];
      const take = Math.min(head.length, n - filled);
      out.set(head.subarray(0, take), filled);
      filled += take;
      this.buffered -= take;
      if (take === head.length) {
        this.queue.shift();
      } else {
        this.queue[0] = head.subarray(take);
      }
    }
    return out;
  }

  /** Drop all buffered audio immediately (barge-in / interrupt). */
  flush(): void {
    this.queue = [];
    this.buffered = 0;
  }
}

/**
 * Rate-limits streaming transcript writes so partial lines update the terminal at most once per
 * `intervalMs`, while finalized lines always flush immediately (SPEC §8.2). `now` is injected so
 * the decision is pure and testable.
 */
export class CommitThrottle {
  private lastCommit = Number.NEGATIVE_INFINITY;

  constructor(private readonly intervalMs: number) {}

  /** Whether the current transcript update should be written now. */
  shouldCommit(now: number, isFinal: boolean): boolean {
    if (isFinal || now - this.lastCommit >= this.intervalMs) {
      this.lastCommit = now;
      return true;
    }
    return false;
  }
}
