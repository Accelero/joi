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
  user: "\x1b[38;2;143;233;223m", // accent cyan
  agent: "\x1b[38;2;226;230;235m", // bright foreground
};
const RESET = "\x1b[0m";

// Resolve a configured theme *name* to concrete xterm colors. This name→colors map is pure
// presentation, so it lives in the frontend; everything else (which theme, font, scrollback) comes
// from the backend `ui` config. Unknown names fall back to `joi-dark`. The palette mirrors the
// minimal-mono deck: near-black field, a monochrome text scale, desaturated ANSI accents.
const THEMES: Record<string, ITheme> = {
  "joi-dark": {
    background: "#090c11",
    foreground: "#e9edf3",
    cursor: "#9aede4",
    cursorAccent: "#090c11",
    selectionBackground: "rgba(143, 233, 223, 0.26)",
    black: "#0b0e12",
    brightBlack: "#6c7682",
    red: "#e08c8c",
    brightRed: "#eba0a0",
    green: "#9fd6c4",
    brightGreen: "#b4e3d3",
    yellow: "#d8c08a",
    brightYellow: "#e6d2a2",
    blue: "#93b2d6",
    brightBlue: "#a8c2e0",
    magenta: "#c3b6e6",
    brightMagenta: "#d2c8ee",
    cyan: "#8fe9df",
    brightCyan: "#a8efe7",
    white: "#e9edf3",
    brightWhite: "#f7f9fb",
  },
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
        const label = speaker === "user" ? "User:" : "JOI:";

        // Starting a line for this speaker (first ever, or speaker changed): close the previous
        // line and open a new labeled one.
        if (openRef.current !== speaker) {
          if (openRef.current !== null) term.write("\r\n");
          term.write(`${SPEAKER_COLOR[speaker]}${label}${RESET} `);
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
        term.write(`\x1b[38;2;224;140;140m! ${message}${RESET}\r\n`);
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

    let term: XTerm | undefined;
    let fit: FitAddon | undefined;
    let disposed = false;
    const onResize = () => fit?.fit();

    const build = () => {
      if (disposed || !container) return;
      term = new XTerm({
        // Explicit monospace stack: the configured font first, then bundled/system mono fallbacks —
        // never a proportional default.
        fontFamily: `"${font}", "JetBrains Mono", ui-monospace, "Cascadia Mono", Menlo, Consolas, monospace`,
        fontSize: 13,
        lineHeight: 1.25,
        letterSpacing: 0,
        scrollback,
        // Read-only transcript: the live cursor lives in the prompt (see `Prompt`), so hide it here.
        cursorBlink: false,
        cursorInactiveStyle: "none",
        disableStdin: true,
        theme: THEMES[theme] ?? THEMES[DEFAULT_THEME],
      });
      fit = new FitAddon();
      term.loadAddon(fit);
      term.open(container);
      fit.fit();
      termRef.current = term;
      window.addEventListener("resize", onResize);
    };

    // Build only once the monospace webfont is loaded. xterm caches the glyph cell width at
    // construction time, so building before the font arrives bakes in the *fallback* metrics — the
    // loaded font then renders against the wrong cell grid and looks non-monospace. The timeout
    // guards the offline / font-never-loads case so we never hang without a terminal.
    const fonts = document.fonts;
    const ready = fonts
      ? Promise.race([
          fonts.load(`13px "${font}"`).then(() => fonts.ready),
          new Promise<void>((resolve) => setTimeout(resolve, 1500)),
        ])
      : Promise.resolve();
    void ready.then(build);

    return () => {
      disposed = true;
      window.removeEventListener("resize", onResize);
      term?.dispose();
      termRef.current = null;
    };
  }, [theme, font, scrollback]);

  return <div ref={containerRef} className="h-full w-full" />;
}
