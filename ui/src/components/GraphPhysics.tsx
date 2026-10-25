import { useCallback, useId, useRef, useState } from "react";
import { Orbit, PinOff, RotateCcw, X } from "lucide-react";

import { Button, Checkbox, IconButton } from "./ui";

/** How the map's force layout behaves. GraphCanvas applies changes live. */
export interface Physics {
  /** How hard a link pulls its two ends together (0–1); hubs answer more softly. */
  follow: number;
  /** Rest length of a link, in canvas units. */
  distance: number;
  /** How hard every node pushes the others away. */
  repulsion: number;
  /** Clear space kept around each node. */
  collision: number;
  /** Velocity lost per tick: low drifts and overshoots, high settles fast. */
  friction: number;
  /** Animate the layout settling; unset follows the system's reduced-motion setting. */
  animate?: boolean;
}

export const DEFAULT_PHYSICS: Physics = { follow: 0.5, distance: 70, repulsion: 180, collision: 10, friction: 0.4 };

const reduceMotion = () => window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;

/** Whether the layout animates as it settles. Without it, it settles in one step; a drag moves nodes either way. */
export const animates = (p: Physics) => p.animate ?? !reduceMotion();

const pct = (v: number) => `${Math.round(v * 100)}%`;
const num = (v: number) => String(v);

const SLIDERS: { key: Exclude<keyof Physics, "animate">; label: string; hint: string; min: number; max: number; step: number; format: (v: number) => string }[] = [
  { key: "follow", label: "Follow", hint: "How much linked nodes follow a dragged one", min: 0.05, max: 1, step: 0.05, format: pct },
  { key: "distance", label: "Link distance", hint: "Spacing between linked nodes", min: 30, max: 220, step: 5, format: num },
  { key: "repulsion", label: "Repulsion", hint: "How hard nodes push each other away", min: 0, max: 600, step: 10, format: num },
  { key: "collision", label: "Collision", hint: "Clear space around each node", min: 0, max: 40, step: 1, format: num },
  { key: "friction", label: "Friction", hint: "Low drifts and overshoots, high settles fast", min: 0.1, max: 0.8, step: 0.05, format: pct },
];

const STORAGE_KEY = "onto.map.physics";

/** Stored settings, clamped to the sliders' ranges; anything unreadable falls back to the default. */
function parse(raw: string | null): Physics {
  const out = { ...DEFAULT_PHYSICS };
  let stored: unknown;
  try {
    stored = JSON.parse(raw ?? "null");
  } catch {
    return out;
  }
  if (!stored || typeof stored !== "object") return out;
  for (const s of SLIDERS) {
    const v = (stored as Record<string, unknown>)[s.key];
    if (typeof v === "number" && Number.isFinite(v)) out[s.key] = Math.min(s.max, Math.max(s.min, v));
  }
  const animate = (stored as Record<string, unknown>).animate;
  if (typeof animate === "boolean") out.animate = animate;
  return out;
}

/** The map's physics, remembered in this browser (storage may be blocked in a private window or a sandboxed frame). */
export function usePhysics(): [Physics, (p: Physics) => void] {
  const [physics, setPhysics] = useState<Physics>(() => {
    try {
      return parse(localStorage.getItem(STORAGE_KEY));
    } catch {
      return DEFAULT_PHYSICS;
    }
  });
  const update = useCallback((p: Physics) => {
    setPhysics(p);
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(p));
    } catch {
      // Not remembered, still applied.
    }
  }, []);
  return [physics, update];
}

const isDefault = (p: Physics) => p.animate === undefined && SLIDERS.every((s) => p[s.key] === DEFAULT_PHYSICS[s.key]);

/**
 * Toolbar button and popover for the map's physics. It stays open while the
 * canvas is used, so a change can be felt by dragging a node straight away.
 */
export function PhysicsSettings({ value, onChange, pinned, onUnpinAll }: { value: Physics; onChange: (p: Physics) => void; pinned: number; onUnpinAll: () => void }) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const id = useId();

  const close = () => {
    setOpen(false);
    trigger.current?.focus();
  };

  return (
    <div className="relative">
      <Button
        ref={trigger}
        size="sm"
        variant={open ? "secondary" : "ghost"}
        icon={<Orbit className="size-4" />}
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        onClick={() => setOpen((o) => !o)}
      >
        Physics
      </Button>
      {open ? (
        <div
          id={id}
          role="dialog"
          aria-label="Map physics"
          onKeyDown={(e) => {
            // Esc closes this first; the entity panel behind it stays open.
            if (e.key === "Escape") {
              e.preventDefault();
              e.stopPropagation();
              close();
            }
          }}
          className="absolute top-full right-0 z-40 mt-2 max-h-[calc(100dvh-4.5rem)] w-[min(300px,calc(100vw-24px))] overflow-y-auto overscroll-contain rounded-card bg-raised p-3 shadow-overlay"
        >
          <div className="mb-2 flex items-center justify-between gap-2">
            <h2 className="text-[13px] font-semibold">Physics</h2>
            <IconButton label="Close" className="-my-1 -mr-1 size-7" onClick={close}>
              <X className="size-4" />
            </IconButton>
          </div>
          <div className="flex flex-col gap-2.5">
            {SLIDERS.map((s) => (
              <div key={s.key}>
                <div className="flex items-baseline justify-between gap-2">
                  <label htmlFor={`${id}-${s.key}`} className="text-[13px] font-medium">
                    {s.label}
                  </label>
                  <span className="tnum text-xs text-ink-2">{s.format(value[s.key])}</span>
                </div>
                <input
                  id={`${id}-${s.key}`}
                  type="range"
                  min={s.min}
                  max={s.max}
                  step={s.step}
                  value={value[s.key]}
                  aria-describedby={`${id}-${s.key}-hint`}
                  aria-valuetext={s.format(value[s.key])}
                  onChange={(e) => onChange({ ...value, [s.key]: Number(e.target.value) })}
                  className="w-full accent-[var(--accent)]"
                />
                <p id={`${id}-${s.key}-hint`} className="text-xs text-ink-3">
                  {s.hint}
                </p>
              </div>
            ))}
          </div>
          <div className="mt-3">
            <Checkbox label="Animate the layout" checked={animates(value)} onChange={(animate) => onChange({ ...value, animate })} />
            <p className="mt-0.5 text-xs text-ink-3">
              {reduceMotion() ? "Off by default because your system asks for reduced motion. " : ""}
              When off, the map settles in one step as you let go.
            </p>
          </div>
          <p className="mt-3 text-xs text-ink-3">Hold Shift as you drop a node to pin it there; Shift-click or P toggles a pin.</p>
          <div className="mt-3 flex items-center justify-between gap-2 border-t border-hairline pt-3">
            <Button size="sm" variant="ghost" icon={<PinOff className="size-4" />} disabled={!pinned} onClick={onUnpinAll}>
              {pinned ? `Unpin ${pinned}` : "Nothing pinned"}
            </Button>
            <Button size="sm" variant="ghost" icon={<RotateCcw className="size-4" />} disabled={isDefault(value)} onClick={() => onChange(DEFAULT_PHYSICS)}>
              Reset
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
