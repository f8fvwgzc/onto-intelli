import type { SchemaDoc } from "./types";

// Kind colours come from the schema (`color` on an object type); kinds without
// one get a stable colour from this ramp (equal lightness, spread hues).
const RAMP = [
  "oklch(62% 0.13 265)",
  "oklch(64% 0.12 230)",
  "oklch(63% 0.12 160)",
  "oklch(70% 0.13 80)",
  "oklch(60% 0.16 25)",
  "oklch(60% 0.14 305)",
  "oklch(64% 0.1 195)",
  "oklch(63% 0.14 350)",
  "oklch(58% 0.08 120)",
  "oklch(55% 0.05 255)",
];

function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export function kindColor(kind: string, schema?: SchemaDoc): string {
  const c = schema?.object_types?.[kind]?.color;
  if (c && /^#[0-9a-fA-F]{6}$/.test(c)) return c;
  return RAMP[hash(kind) % RAMP.length];
}

export function kindLabel(kind: string, schema?: SchemaDoc): string {
  const l = schema?.object_types?.[kind]?.label;
  if (l) return l;
  return kind.replace(/_/g, " ").replace(/^\w/, (c) => c.toUpperCase());
}

const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

export function ago(ms: number): string {
  const diff = ms - Date.now();
  const abs = Math.abs(diff);
  const units: [Intl.RelativeTimeFormatUnit, number][] = [
    ["year", 31536e6],
    ["month", 2592e6],
    ["day", 864e5],
    ["hour", 36e5],
    ["minute", 6e4],
  ];
  for (const [unit, size] of units) if (abs >= size) return rtf.format(Math.round(diff / size), unit);
  return "just now";
}

const dtf = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

export function when(ms: number): string {
  return dtf.format(new Date(ms));
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n.toLocaleString()} ${n === 1 ? one : many}`;
}

export function shortId(id: string): string {
  const i = id.indexOf(":");
  return i < 0 ? id : id.slice(i + 1);
}

export function display(v: unknown): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return JSON.stringify(v);
}

// ── numbers for the console (metrics, costs, latencies) ─────────────────────

const compact = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });

/** 1234 → "1.2K"; small numbers stay exact. */
export function num(n: number): string {
  return Math.abs(n) < 10_000 ? Math.round(n).toLocaleString() : compact.format(n);
}

/** Milliseconds as "41 ms" or "1.83 s". */
export function ms(v: number): string {
  if (!Number.isFinite(v)) return "–";
  if (v === 0) return "0 ms";
  if (v < 1) return `${v.toFixed(2)} ms`;
  if (v < 1000) return `${Math.round(v)} ms`;
  return `${(v / 1000).toFixed(v < 10_000 ? 2 : 1)} s`;
}

/** US dollars with enough digits for fractions of a cent. */
export function usd(v: number): string {
  if (v === 0) return "$0";
  const digits = Math.abs(v) >= 1 ? 2 : Math.abs(v) >= 0.01 ? 3 : 5;
  return `$${v.toLocaleString(undefined, { minimumFractionDigits: Math.min(digits, 2), maximumFractionDigits: digits })}`;
}

/** 0.0058 → "0.58%". */
export function pct(v: number): string {
  const p = v * 100;
  return `${p >= 10 || p === 0 ? p.toFixed(0) : p.toFixed(p >= 1 ? 1 : 2)}%`;
}

/** The change from `prev` to `cur` in words: "+12% vs previous", "no change", "new". */
export function delta(cur: number, prev: number, lowerIsBetter = false): { text: string; tone: "good" | "bad" | "flat" } {
  if (prev === 0 && cur === 0) return { text: "no change", tone: "flat" };
  if (prev === 0) return { text: "new this period", tone: "flat" };
  const d = (cur - prev) / Math.abs(prev);
  if (Math.abs(d) < 0.005) return { text: "no change", tone: "flat" };
  const up = d > 0;
  const better = lowerIsBetter ? !up : up;
  return { text: `${up ? "+" : "−"}${pct(Math.abs(d))} vs previous`, tone: better ? "good" : "bad" };
}
