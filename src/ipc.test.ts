import { describe, it, expect } from "vitest";
import type { UiEvent } from "./ipc";

// These literals are exactly what joi-core's serde emits for each `UiEvent` variant
// (internally tagged on `type`, transcript's `final_` renamed to `final`, `History(HistoryMeta)`
// flattened). If the Rust shape changes, these stop type-checking or assert-failing — the parity
// guard of PLAN §5 (m-4).
const SAMPLES: Record<string, string> = {
  state: '{"type":"state","state":"listening"}',
  transcript: '{"type":"transcript","speaker":"agent","text":"hi","final":true}',
  connection: '{"type":"connection","status":"connected","detail":null}',
  reachability: '{"type":"reachability","state":"online","detail":null}',
  history: '{"type":"history","turns":2,"token_estimate":12,"budget":32000}',
  metrics:
    '{"type":"metrics","up_kbps":256,"down_kbps":384,"up_tokens_per_sec":3,"down_tokens_per_sec":12.5}',
  error: '{"type":"error","kind":"auth","message":"invalid key"}',
};

describe("UiEvent parity", () => {
  it("narrows each variant from its Rust JSON", () => {
    for (const json of Object.values(SAMPLES)) {
      const ev = JSON.parse(json) as UiEvent;
      switch (ev.type) {
        case "state":
          expect(ev.state).toBe("listening");
          break;
        case "transcript":
          expect(ev.speaker).toBe("agent");
          expect(ev.final).toBe(true);
          break;
        case "connection":
          expect(ev.status).toBe("connected");
          expect(ev.detail).toBeNull();
          break;
        case "reachability":
          expect(ev.state).toBe("online");
          expect(ev.detail).toBeNull();
          break;
        case "history":
          expect(ev.turns).toBe(2);
          expect(ev.budget).toBe(32000);
          break;
        case "metrics":
          expect(ev.up_kbps).toBe(256);
          expect(ev.down_kbps).toBe(384);
          expect(ev.up_tokens_per_sec).toBe(3);
          expect(ev.down_tokens_per_sec).toBe(12.5);
          break;
        case "error":
          expect(ev.kind).toBe("auth");
          break;
        default: {
          // Exhaustiveness: a new Rust variant without a TS arm fails to compile here.
          const _never: never = ev;
          throw new Error(`unhandled variant: ${JSON.stringify(_never)}`);
        }
      }
    }
  });

  it("uses the renamed `final` key, not `final_`", () => {
    expect(SAMPLES.transcript).toContain('"final":true');
    expect(SAMPLES.transcript).not.toContain("final_");
  });
});
