/**
 * Inline SVG icons for the control surface. Hand-rolled (no icon-library dependency) and drawn with
 * `currentColor`, so each one inherits the button's state color (accent / danger / dim). Stroke
 * icons share one geometry style; play/stop are filled. Purely presentational — `aria-hidden`, with
 * the accessible label living on the parent button.
 */
type IconProps = { size?: number; className?: string };

const stroke = {
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.75,
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
};

function Svg({ size = 17, className, children }: IconProps & { children: React.ReactNode }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      className={className}
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

export function PlayIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <polygon points="7 4.5 20 12 7 19.5" fill="currentColor" />
    </Svg>
  );
}

export function StopIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <rect x="6" y="6" width="12" height="12" rx="1.5" fill="currentColor" />
    </Svg>
  );
}

export function MicIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <rect x="9" y="2.5" width="6" height="11" rx="3" {...stroke} />
      <path d="M5.5 10.5a6.5 6.5 0 0 0 13 0" {...stroke} />
      <line x1="12" y1="17" x2="12" y2="21" {...stroke} />
      <line x1="8.5" y1="21" x2="15.5" y2="21" {...stroke} />
    </Svg>
  );
}

export function MicOffIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <path d="M15 4.4A3 3 0 0 0 9 5v3m0 3.5V8" {...stroke} />
      <path d="M15 9.5v1a3 3 0 0 1-4.6 2.5" {...stroke} />
      <path d="M5.5 10.5a6.5 6.5 0 0 0 10.2 5.3M18.5 10.5a6.4 6.4 0 0 1-.4 2.2" {...stroke} />
      <line x1="12" y1="17" x2="12" y2="21" {...stroke} />
      <line x1="8.5" y1="21" x2="15.5" y2="21" {...stroke} />
      <line x1="3.5" y1="3" x2="20.5" y2="21" {...stroke} />
    </Svg>
  );
}

export function MonitorIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <rect x="3" y="4" width="18" height="13" rx="2" {...stroke} />
      <line x1="8" y1="21" x2="16" y2="21" {...stroke} />
      <line x1="12" y1="17" x2="12" y2="21" {...stroke} />
    </Svg>
  );
}

export function SendIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <path d="M20 5v5a4 4 0 0 1-4 4H5" {...stroke} />
      <polyline points="9 10 4.5 14 9 18" {...stroke} />
    </Svg>
  );
}

// ── Window-control icons (custom titlebar) ────────────────────────────────────────────────────
export function MinimizeIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <line x1="5" y1="12" x2="19" y2="12" {...stroke} />
    </Svg>
  );
}

export function MaximizeIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <rect x="5.5" y="5.5" width="13" height="13" rx="1.5" {...stroke} />
    </Svg>
  );
}

export function CloseIcon(p: IconProps): React.JSX.Element {
  return (
    <Svg {...p}>
      <line x1="6" y1="6" x2="18" y2="18" {...stroke} />
      <line x1="18" y1="6" x2="6" y2="18" {...stroke} />
    </Svg>
  );
}
