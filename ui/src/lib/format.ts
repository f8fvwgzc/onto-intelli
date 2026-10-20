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
