import { describe, it, expect } from "vitest";
import {
  CommitThrottle,
  FrameAccumulator,
  JitterBuffer,
  downsample,
  floatToPcm16,
  samplesPerFrame,
  TARGET_INPUT_RATE,
  OUTPUT_RATE,
} from "./dsp";

describe("samplesPerFrame", () => {
  it("is 320 samples for 20 ms at 16 kHz", () => {
    expect(samplesPerFrame(TARGET_INPUT_RATE, 20)).toBe(320);
    expect(samplesPerFrame(OUTPUT_RATE, 20)).toBe(480);
  });
});

describe("floatToPcm16", () => {
  it("maps the float range to PCM16 and clamps overshoot", () => {
    const out = floatToPcm16(new Float32Array([0, 1, -1, 2, -2, 0.5]));
    expect(out[0]).toBe(0);
    expect(out[1]).toBe(32767);
    expect(out[2]).toBe(-32768);
    expect(out[3]).toBe(32767); // clamped
    expect(out[4]).toBe(-32768); // clamped
    expect(out[5]).toBe(16383); // 0.5 * 32767 = 16383.5, truncated by Int16Array
  });
});

describe("downsample", () => {
  it("halves length when going 32k -> 16k", () => {
    const input = new Float32Array(100).map((_, i) => Math.sin(i / 5));
    const out = downsample(input, 32_000, 16_000);
    expect(out.length).toBe(50);
  });

  it("passes through when target >= input rate", () => {
    const input = new Float32Array([0.1, 0.2, 0.3]);
    const out = downsample(input, 16_000, 16_000);
    expect(out.length).toBe(3);
    expect(out[0]).toBeCloseTo(0.1, 6);
    expect(out[2]).toBeCloseTo(0.3, 6);
  });

  it("rejects non-positive rates", () => {
    expect(() => downsample(new Float32Array([0]), 0, 16_000)).toThrow(RangeError);
  });
});

describe("FrameAccumulator", () => {
  it("emits full frames and buffers the remainder across pushes", () => {
    const acc = new FrameAccumulator(320);
    expect(acc.push(new Int16Array(200))).toHaveLength(0);
    expect(acc.bufferedSamples).toBe(200);

    const frames = acc.push(new Int16Array(500)); // 700 total -> two frames, 60 remainder
    expect(frames).toHaveLength(2);
    expect(frames[0].length).toBe(320);
    expect(acc.bufferedSamples).toBe(60);
  });

  it("rejects a non-positive frame size", () => {
    expect(() => new FrameAccumulator(0)).toThrow(RangeError);
  });
});

describe("JitterBuffer", () => {
  it("pulls across chunk boundaries and reports buffered ms", () => {
    const jb = new JitterBuffer();
    jb.enqueue(Int16Array.from([1, 2, 3]));
    jb.enqueue(Int16Array.from([4, 5]));
    expect(jb.bufferedSamples).toBe(5);

    const got = jb.pull(4);
    expect(Array.from(got)).toEqual([1, 2, 3, 4]);
    expect(jb.bufferedSamples).toBe(1);
  });

  it("pads with silence on underrun", () => {
    const jb = new JitterBuffer();
    jb.enqueue(Int16Array.from([7, 8]));
    const got = jb.pull(5);
    expect(Array.from(got)).toEqual([7, 8, 0, 0, 0]);
    expect(jb.bufferedSamples).toBe(0);
  });

  it("flush drops everything immediately (barge-in)", () => {
    const jb = new JitterBuffer();
    jb.enqueue(new Int16Array(2400)); // 100 ms at 24k
    expect(jb.bufferedMs()).toBeCloseTo(100, 5);
    jb.flush();
    expect(jb.bufferedSamples).toBe(0);
    expect(Array.from(jb.pull(2))).toEqual([0, 0]);
  });
});

describe("CommitThrottle", () => {
  it("commits finals immediately and rate-limits partials", () => {
    const t = new CommitThrottle(100);
    expect(t.shouldCommit(0, false)).toBe(true); // first partial
    expect(t.shouldCommit(50, false)).toBe(false); // within interval
    expect(t.shouldCommit(150, false)).toBe(true); // interval elapsed
    expect(t.shouldCommit(160, true)).toBe(true); // final always commits
    expect(t.shouldCommit(170, false)).toBe(false); // reset by the final at 160
  });
});
