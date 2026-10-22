// Small SVG charts for the console: line (with area), stacked bars, sparkline,
// latency histogram and a horizontal bar list. No chart library; colours come
// from CSS variables (--chart-*, --chart-grid), so light and dark follow the
// tokens. Line and bar charts take the pointer or the arrow keys (focus the
// chart) and show the values of one bucket.

import { useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type PointerEvent, type ReactNode } from "react";

import { ms as fmtMs, num } from "../lib/format";
import { cx } from "./ui";

export const SERIES = ["var(--chart-1)", "var(--chart-2)", "var(--chart-3)", "var(--chart-4)", "var(--chart-5)", "var(--chart-6)"];

function useWidth<T extends HTMLElement>(): [React.RefObject<T | null>, number] {
  const ref = useRef<T>(null);
  const [w, setW] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    setW(el.clientWidth);
    const ro = new ResizeObserver((entries) => setW(Math.floor(entries[0].contentRect.width)));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [ref, w];
}

/** The smallest of 1, 2, 2.5, 5 × 10ⁿ at or above `v`. */
function niceMax(v: number): number {
  if (!(v > 0)) return 1;
  const p = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= v) return m * p;
  return 10 * p;
}

const monthDay = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", timeZone: "UTC" });

/** A metrics bucket label (`2026-10-21` or `2026-10-21T13:00`, UTC) for an axis. */
export function bucketLabel(b: string): string {
  if (b.includes("T")) return b.slice(11, 16);
  const d = new Date(`${b}T00:00:00Z`);
  return Number.isNaN(d.getTime()) ? b : monthDay.format(d);
}

function axisTicks(n: number): number[] {
  if (n <= 0) return [];
  if (n <= 2) return [...Array(n).keys()];
  return [...new Set([0, Math.floor((n - 1) / 2), n - 1])];
}

function NoData({ height, children = "No data in this range" }: { height: number; children?: ReactNode }) {
  return (
    <div className="grid place-items-center rounded-input bg-sunken text-[13px] text-ink-3" style={{ height }}>
      {children}
    </div>
  );
}

function Legend({ items }: { items: { name: string; color: string; dashed?: boolean }[] }) {
  if (items.length < 2) return null;
  return (
    <ul className="mb-2 flex flex-wrap gap-x-3 gap-y-1 text-xs text-ink-2">
      {items.map((s) => (
        <li key={s.name} className="inline-flex items-center gap-1.5">
          <span aria-hidden className={cx("inline-block h-0.5 w-3 rounded-full", s.dashed && "opacity-60")} style={{ background: s.color }} />
          {s.name}
        </li>
      ))}
    </ul>
  );
}

interface Hover {
  index: number | null;
  onMove: (e: PointerEvent<SVGSVGElement>) => void;
  onLeave: () => void;
  onKey: (e: KeyboardEvent<SVGSVGElement>) => void;
  onBlur: () => void;
}

function useHover(n: number, indexAt: (px: number) => number): Hover {
  const [index, setIndex] = useState<number | null>(null);
  return {
    index: index !== null && index < n ? index : null,
    onMove: (e) => {
      const r = e.currentTarget.getBoundingClientRect();
      setIndex(Math.max(0, Math.min(n - 1, indexAt(e.clientX - r.left))));
    },
    onLeave: () => setIndex(null),
    onBlur: () => setIndex(null),
    onKey: (e) => {
      if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
        e.preventDefault();
        const step = e.key === "ArrowRight" ? 1 : -1;
        setIndex((i) => Math.max(0, Math.min(n - 1, (i ?? (step > 0 ? -1 : n)) + step)));
      } else if (e.key === "Escape") {
        setIndex(null);
      }
    },
  };
}

function Tooltip({ x, width, title, rows }: { x: number; width: number; title: string; rows: { name: string; color: string; value: string }[] }) {
  const left = Math.max(0, Math.min(width - 168, x - 84));
  return (
    <div className="pointer-events-none absolute top-0 z-10 w-[168px] rounded-input bg-raised px-2.5 py-2 text-xs shadow-overlay" style={{ left }}>
      <p className="mb-1 font-medium text-ink">{title}</p>
      {rows.map((r) => (
        <p key={r.name} className="flex items-center gap-1.5 text-ink-2">
          <span aria-hidden className="inline-block size-2 rounded-full" style={{ background: r.color }} />
          <span className="min-w-0 flex-1 truncate">{r.name}</span>
          <span className="tnum text-ink">{r.value}</span>
        </p>
      ))}
    </div>
  );
}

const PAD = { l: 44, r: 8, t: 8, b: 22 };

export interface LineSeries {
  name: string;
  values: number[];
  color: string;
  dashed?: boolean;
}

/** A line chart over buckets; the first series gets a fading area. */
export function LineChart({
  labels,
  series,
  height = 180,
  format = num,
  label,
  area = true,
}: {
  labels: string[];
  series: LineSeries[];
  height?: number;
  format?: (v: number) => string;
  label: string;
  area?: boolean;
}) {
  const [ref, w] = useWidth<HTMLDivElement>();
  const gid = `g${useId().replace(/[^a-zA-Z0-9]/g, "")}`;
  const n = labels.length;
  const iw = Math.max(0, w - PAD.l - PAD.r);
  const ih = height - PAD.t - PAD.b;
  const max = niceMax(Math.max(0, ...series.flatMap((s) => s.values)));
  const x = (i: number) => PAD.l + (n <= 1 ? iw / 2 : (i * iw) / (n - 1));
  const y = (v: number) => PAD.t + ih - (Math.max(0, v) / max) * ih;
  const hover = useHover(n, (px) => (n <= 1 ? 0 : Math.round(((px - PAD.l) / Math.max(1, iw)) * (n - 1))));
  const empty = n === 0 || series.every((s) => s.values.every((v) => !v));

  return (
    <div ref={ref} className="relative min-w-0">
      {empty ? null : <Legend items={series} />}
      {empty ? (
        <NoData height={height} />
      ) : w > 0 ? (
        <svg
          width={w}
          height={height}
          role="img"
          aria-label={label}
          tabIndex={0}
          className="block touch-pan-y rounded-input outline-offset-2"
          onPointerMove={hover.onMove}
          onPointerLeave={hover.onLeave}
          onKeyDown={hover.onKey}
          onBlur={hover.onBlur}
        >
          <defs>
            <linearGradient id={gid} x1="0" x2="0" y1="0" y2="1">
              <stop offset="0%" stopColor={series[0]?.color} stopOpacity="0.15" />
              <stop offset="100%" stopColor={series[0]?.color} stopOpacity="0" />
            </linearGradient>
          </defs>
          {[0, 0.5, 1].map((f) => (
            <g key={f}>
              <line x1={PAD.l} x2={w - PAD.r} y1={y(max * f)} y2={y(max * f)} stroke="var(--chart-grid)" strokeWidth="1" />
              <text x={PAD.l - 6} y={y(max * f)} dy="0.32em" textAnchor="end" className="tnum fill-[var(--text-3)] text-[10.5px]">
                {format(max * f)}
              </text>
            </g>
          ))}
          {axisTicks(n).map((i) => (
            <text key={i} x={x(i)} y={height - 6} textAnchor={n > 1 && i === 0 ? "start" : n > 1 && i === n - 1 ? "end" : "middle"} className="fill-[var(--text-3)] text-[10.5px]">
              {bucketLabel(labels[i])}
            </text>
          ))}
          {series.map((s, si) => {
            const d = s.values.map((v, i) => `${i ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join("");
            return (
              <g key={s.name}>
                {si === 0 && area && n > 1 ? <path d={`${d}L${x(n - 1).toFixed(1)},${PAD.t + ih}L${x(0).toFixed(1)},${PAD.t + ih}Z`} fill={`url(#${gid})`} /> : null}
                <path d={d} fill="none" stroke={s.color} strokeWidth="1.75" strokeLinejoin="round" strokeLinecap="round" strokeDasharray={s.dashed ? "4 3" : undefined} />
                {n === 1 ? <circle cx={x(0)} cy={y(s.values[0])} r="3" fill={s.color} /> : null}
              </g>
            );
          })}
          {hover.index !== null ? (
            <g>
              <line x1={x(hover.index)} x2={x(hover.index)} y1={PAD.t} y2={PAD.t + ih} stroke="var(--text-3)" strokeWidth="1" strokeDasharray="2 3" />
              {series.map((s) => (
                <circle key={s.name} cx={x(hover.index!)} cy={y(s.values[hover.index!] ?? 0)} r="3" fill="var(--raised)" stroke={s.color} strokeWidth="1.75" />
              ))}
            </g>
          ) : null}
        </svg>
      ) : (
        <div style={{ height }} />
      )}
      {hover.index !== null && !empty ? (
        <Tooltip
          x={x(hover.index)}
          width={w}
          title={bucketLabel(labels[hover.index])}
          rows={series.map((s) => ({ name: s.name, color: s.color, value: format(s.values[hover.index!] ?? 0) }))}
        />
      ) : null}
    </div>
  );
}

export interface BarStack {
  name: string;
  values: number[];
  color: string;
}

/** Stacked bars, one column per bucket (daily usage by model). */
export function StackedBars({
  labels,
  stacks,
  height = 200,
  format = num,
  label,
}: {
  labels: string[];
  stacks: BarStack[];
  height?: number;
  format?: (v: number) => string;
  label: string;
}) {
  const [ref, w] = useWidth<HTMLDivElement>();
  const n = labels.length;
  const iw = Math.max(0, w - PAD.l - PAD.r);
  const ih = height - PAD.t - PAD.b;
  const totals = labels.map((_, i) => stacks.reduce((a, s) => a + (s.values[i] ?? 0), 0));
  const max = niceMax(Math.max(0, ...totals));
  const col = n ? iw / n : 0;
  const bw = Math.max(2, Math.min(28, col * 0.68));
  const y = (v: number) => PAD.t + ih - (v / max) * ih;
  const hover = useHover(n, (px) => Math.floor((px - PAD.l) / Math.max(1, col)));
  const empty = totals.every((t) => !t);

  return (
    <div ref={ref} className="relative min-w-0">
      {empty ? null : <Legend items={stacks} />}
      {empty ? (
        <NoData height={height} />
      ) : w > 0 ? (
        <svg
          width={w}
          height={height}
          role="img"
          aria-label={label}
          tabIndex={0}
          className="block touch-pan-y rounded-input"
          onPointerMove={hover.onMove}
          onPointerLeave={hover.onLeave}
          onKeyDown={hover.onKey}
          onBlur={hover.onBlur}
        >
          {[0, 0.5, 1].map((f) => (
            <g key={f}>
              <line x1={PAD.l} x2={w - PAD.r} y1={y(max * f)} y2={y(max * f)} stroke="var(--chart-grid)" strokeWidth="1" />
              <text x={PAD.l - 6} y={y(max * f)} dy="0.32em" textAnchor="end" className="tnum fill-[var(--text-3)] text-[10.5px]">
                {format(max * f)}
              </text>
            </g>
          ))}
          {labels.map((l, i) => {
            let acc = 0;
            const cx0 = PAD.l + i * col + (col - bw) / 2;
            return (
              <g key={l} opacity={hover.index === null || hover.index === i ? 1 : 0.45}>
                {stacks.map((s) => {
                  const v = s.values[i] ?? 0;
                  if (!v) return null;
                  const top = y(acc + v);
                  const h = y(acc) - top;
                  acc += v;
                  return <rect key={s.name} x={cx0} y={top} width={bw} height={Math.max(1, h - 1)} rx="1.5" fill={s.color} />;
                })}
              </g>
            );
          })}
          {axisTicks(n).map((i) => (
            <text key={i} x={PAD.l + i * col + col / 2} y={height - 6} textAnchor="middle" className="fill-[var(--text-3)] text-[10.5px]">
              {bucketLabel(labels[i])}
            </text>
          ))}
        </svg>
      ) : (
        <div style={{ height }} />
      )}
      {hover.index !== null && !empty ? (
        <Tooltip
          x={PAD.l + hover.index * col + col / 2}
          width={w}
          title={`${bucketLabel(labels[hover.index])} · ${format(totals[hover.index])}`}
          rows={stacks.filter((s) => s.values[hover.index!]).map((s) => ({ name: s.name, color: s.color, value: format(s.values[hover.index!]) }))}
        />
      ) : null}
    </div>
  );
}

/** A trend line without axes, for metric cards. */
export function Sparkline({ values, color = "var(--chart-1)", height = 32, label }: { values: number[]; color?: string; height?: number; label?: string }) {
  const gid = `s${useId().replace(/[^a-zA-Z0-9]/g, "")}`;
  if (values.length < 2) return <div style={{ height }} aria-hidden />;
  const max = Math.max(...values) || 1;
  const pts = values.map((v, i) => `${((i / (values.length - 1)) * 100).toFixed(2)},${(height - 2 - (v / max) * (height - 4)).toFixed(2)}`);
  return (
    <svg
      viewBox={`0 0 100 ${height}`}
      preserveAspectRatio="none"
      width="100%"
      height={height}
      role={label ? "img" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
    >
      <defs>
        <linearGradient id={gid} x1="0" x2="0" y1="0" y2="1">
          <stop offset="0%" stopColor={color} stopOpacity="0.15" />
          <stop offset="100%" stopColor={color} stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={`M${pts.join("L")}L100,${height}L0,${height}Z`} fill={`url(#${gid})`} />
      <polyline points={pts.join(" ")} fill="none" stroke={color} strokeWidth="1.5" vectorEffect="non-scaling-stroke" strokeLinejoin="round" />
    </svg>
  );
}

/** Latency distribution: log-spaced bins when the range spans a decade or more. */
export function Histogram({ values, bins = 18, height = 72, label }: { values: number[]; bins?: number; height?: number; label: string }) {
  const [ref, w] = useWidth<HTMLDivElement>();
  if (!values.length) return <NoData height={height}>No traces</NoData>;
  const lo = Math.max(0.1, Math.min(...values));
  const hi = Math.max(lo * 1.0001, Math.max(...values));
  const log = hi / lo >= 10;
  const edge = (i: number) => (log ? lo * (hi / lo) ** (i / bins) : lo + ((hi - lo) * i) / bins);
  const counts = new Array<number>(bins).fill(0);
  for (const v of values) {
    const f = log ? Math.log(Math.max(v, lo) / lo) / Math.log(hi / lo) : (v - lo) / (hi - lo);
    counts[Math.min(bins - 1, Math.max(0, Math.floor(f * bins)))]++;
  }
  const max = Math.max(...counts);
  const bw = w / bins;
  return (
    <div ref={ref} className="min-w-0">
      {w > 0 ? (
        <svg width={w} height={height + 16} role="img" aria-label={label} className="block">
          {counts.map((c, i) => {
            const h = c ? Math.max(2, (c / max) * height) : 0;
            return (
              <rect key={i} x={i * bw + 1} y={height - h} width={Math.max(1, bw - 2)} height={h} rx="1.5" fill="var(--chart-1)" opacity={0.35 + 0.65 * (c / max)}>
                <title>{`${fmtMs(edge(i))} – ${fmtMs(edge(i + 1))}: ${c} trace${c === 1 ? "" : "s"}`}</title>
              </rect>
            );
          })}
          <line x1="0" x2={w} y1={height + 0.5} y2={height + 0.5} stroke="var(--chart-grid)" />
          <text x="0" y={height + 13} className="tnum fill-[var(--text-3)] text-[10.5px]">
            {fmtMs(lo)}
          </text>
          <text x={w} y={height + 13} textAnchor="end" className="tnum fill-[var(--text-3)] text-[10.5px]">
            {fmtMs(hi)}
          </text>
        </svg>
      ) : (
        <div style={{ height: height + 16 }} />
      )}
    </div>
  );
}

/** Horizontal bars with direct labels (cost by model, requests by name). */
export function BarList({
  rows,
  format = num,
  empty = "Nothing yet",
}: {
  rows: { label: string; value: number; hint?: string }[];
  format?: (v: number) => string;
  empty?: string;
}) {
  if (!rows.length) return <NoData height={96}>{empty}</NoData>;
  const max = Math.max(...rows.map((r) => r.value)) || 1;
  return (
    <ul className="flex flex-col gap-2">
      {rows.map((r) => (
        <li key={r.label} className="min-w-0">
          <div className="flex items-baseline gap-2 text-[13px]">
            <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-ink" title={r.label}>
              {r.label}
            </span>
            {r.hint ? <span className="text-xs text-ink-3">{r.hint}</span> : null}
            <span className="tnum font-medium text-ink">{format(r.value)}</span>
          </div>
          <div className="mt-1 h-1.5 rounded-full bg-sunken">
            <div className="h-1.5 rounded-full bg-accent" style={{ width: `${Math.max(1.5, (r.value / max) * 100)}%` }} />
          </div>
        </li>
      ))}
    </ul>
  );
}
