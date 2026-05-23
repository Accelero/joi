/**
 * The conversation terminal (SPEC §8.2): an xterm.js surface that renders streaming transcripts
 * with per-speaker ANSI colors. Partial lines are rewritten in place (`\x1b[2K\r`) and committed
 * with a newline when finalized. Media never touches this component — only text.
 */
import { useEffect, useImperativeHandle, useRef, type Ref } from "react";
import { Terminal as XTerm, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import type { Speaker, TerminalCfg } from "../ipc";

/** Imperative surface the parent drives from `UiEvent`s. */
export interface TerminalHandle {
  /** Write/replace a transcript line. Partials rewrite in place; `final` commits with a newline. */
  writeTranscript(speaker: Speaker, text: string, final: boolean): void;
  /** Write a surfaced error line. */
  writeError(message: string): void;
  /** Clear the screen. */
  clear(): void;
}

const SPEAKER_COLOR: Record<Speaker, string> = {
  user: "\x1b[36m", // cyan
  agent: "\x1b[32m", // green
};
const RESET = "\x1b[0m";

// Resolve a configured theme *name* to concrete xterm colors. This name→colors map is pure
// presentation, so it lives in the frontend; everything else (which theme, font, scrollback) comes
// from the backend `ui` config. Unknown names fall back to `joi-dark`.
const THEMES: Record<string, ITheme> = {
  "joi-dark": { background: "#0b0f14", foreground: "#cbd5e1" },
};
const DEFAULT_THEME = "joi-dark";

export function Terminal({
  ref,
  terminal,
}: {
  ref?: Ref<TerminalHandle>;
  terminal?: TerminalCfg;
}): React.JSX.Element {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<XTerm | null>(null);
  // Speaker of the line currently being streamed, or null when no line is open.
  const openRef = useRef<Speaker | null>(null);

  useImperativeHandle(
    ref,
    (): TerminalHandle => ({
      writeTranscript(speaker, text, final) {
        const term = termRef.current;
        if (!term) return;
        const label = speaker === "user" ? "you" : "joi";

        // Starting a line for this speaker (first ever, or speaker changed): close the previous
        // line and open a new labeled one.
        if (openRef.current !== speaker) {
          if (openRef.current !== null) term.write("\r\n");
          term.write(`${SPEAKER_COLOR[speaker]}${label}${RESET}  `);
          openRef.current = speaker;
        }

        // `text` is an incremental delta — append it (wraps naturally, no redraw).
        if (text.length > 0) term.write(text);

        if (final) {
          term.write("\r\n");
          openRef.current = null;
        }
      },
      writeError(message) {
        const term = termRef.current;
        if (!term) return;
        if (openRef.current !== null) {
          term.write("\r\n");
          openRef.current = null;
        }
        term.write(`\x1b[31m! ${message}${RESET}\r\n`);
      },
      clear() {
        termRef.current?.clear();
        openRef.current = null;
      },
    }),
    [],
  );

  // Recreate the xterm instance when the backend `ui` config arrives (it loads once, async, shortly
  // after mount — before any transcript is written, so nothing is lost).
  const theme = terminal?.theme ?? DEFAULT_THEME;
  const font = terminal?.font ?? "JetBrains Mono";
  const scrollback = terminal?.scrollback ?? 5000;
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const term = new XTerm({
      fontFamily: `${font}, monospace`,
      fontSize: 14,
      scrollback,
      cursorBlink: false,
      disableStdin: true,
      theme: THEMES[theme] ?? THEMES[DEFAULT_THEME],
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);
    fit.fit();
    term.writeln("\x1b[2mJoi ready — press Start to talk.\x1b[0m");
    termRef.current = term;

    const onResize = () => fit.fit();
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
      term.dispose();
      termRef.current = null;
    };
  }, [theme, font, scrollback]);

  return <div ref={containerRef} className="h-full w-full" />;
}
