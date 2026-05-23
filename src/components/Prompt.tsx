/**
 * The terminal's bottom region: a JOI status line and a TUI-style input prompt, both pinned below
 * the (scrolling, read-only) transcript inside the same panel so it reads as one terminal.
 *
 * The block cursor is the "mirrored textarea" technique: a transparent <textarea> handles all real
 * editing (keys, selection, paste, wrapping) while an aria-hidden display layer renders the same
 * text plus an inverted, blinking block caret at the caret index. Both layers share identical font
 * metrics and wrapping, so the fake caret lands exactly where the real one is. The prompt grows as
 * input wraps (the display is in normal flow; the textarea is absolutely overlaid on it).
 *
 * Pure presentation/input — no business logic. `onSend` hands the trimmed line to the parent.
 */
import { useEffect, useRef, useState } from "react";
import type { AppState } from "../ipc";

interface PromptProps {
  state: AppState;
  /** Whether a session is live (text can be sent). The cursor blinks regardless. */
  canSend: boolean;
  onSend(text: string): void;
}

// Per-state color + dot animation for the in-terminal status line. `glow-soft` is a calm pulse
// (listening); `glow-active` is a faster, scaling pulse (speaking); transient states blink.
const STATUS: Record<AppState, { color: string; anim: string }> = {
  stopped: { color: "var(--color-fg-faint)", anim: "none" },
  connecting: { color: "var(--color-warn)", anim: "dot-blink 1s ease-in-out infinite" },
  listening: { color: "var(--color-accent)", anim: "dot-glow-soft 2s ease-in-out infinite" },
  thinking: { color: "var(--color-think)", anim: "dot-glow-soft 1.05s ease-in-out infinite" },
  speaking: { color: "var(--color-speak)", anim: "dot-glow-active 0.85s ease-in-out infinite" },
  reconnecting: { color: "var(--color-warn)", anim: "dot-blink 1s ease-in-out infinite" },
  error: { color: "var(--color-danger)", anim: "none" },
};

export function Prompt({ state, canSend, onSend }: PromptProps): React.JSX.Element {
  const taRef = useRef<HTMLTextAreaElement>(null);
  const [value, setValue] = useState("");
  const [caret, setCaret] = useState(0);

  // Keep the rendered caret index in sync with the textarea's real selection (typing, arrows,
  // clicks, paste all funnel through here).
  const syncCaret = () => setCaret(taRef.current?.selectionStart ?? value.length);

  useEffect(() => {
    taRef.current?.focus();
  }, []);

  const submit = () => {
    const text = value.trim();
    if (!text || !canSend) return;
    onSend(text);
    setValue("");
    setCaret(0);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter sends; Shift+Enter inserts a newline (handled natively).
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  };

  const { color, anim } = STATUS[state];

  // Split the line for the mirrored display: text before the caret, the single char *under* it
  // (rendered inverted), then the rest. A non-breaking space gives the caret width at end of line.
  const before = value.slice(0, caret);
  const under = value.slice(caret, caret + 1);
  const after = value.slice(caret + 1);

  return (
    <div className="tui-foot">
      <div className="tui-status">
        <span className="tui-dot" style={{ color, animation: anim }} aria-hidden />
        <span className="tui-state" style={{ color }}>
          {state}
        </span>
      </div>

      <div className="tui-prompt" onClick={() => taRef.current?.focus()}>
        <span className="tui-chevron" aria-hidden>
          ❯
        </span>
        <div className="tui-input-wrap">
          <div className="tui-display" aria-hidden>
            {before}
            <span className="tui-caret">{under || " "}</span>
            {after}
            {value === "" && <span className="tui-placeholder">message JOI…</span>}
          </div>
          <textarea
            ref={taRef}
            className="tui-input"
            value={value}
            rows={1}
            spellCheck={false}
            autoComplete="off"
            autoCapitalize="off"
            aria-label="Message JOI"
            onChange={(e) => {
              setValue(e.target.value);
              setCaret(e.target.selectionStart ?? e.target.value.length);
            }}
            onKeyDown={onKeyDown}
            onSelect={syncCaret}
            onKeyUp={syncCaret}
            onClick={syncCaret}
            onFocus={syncCaret}
          />
        </div>
      </div>
    </div>
  );
}
