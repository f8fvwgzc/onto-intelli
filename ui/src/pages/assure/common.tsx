import type { ReactNode } from "react";
import { CircleAlert, CircleCheck, CircleHelp, Download, TriangleAlert } from "lucide-react";

import { Button, cx } from "../../components/ui";

/** What a report is (a measurement, an opinion, a proof), in the engine's own words. */
export function Means({ children }: { children?: string }) {
  if (!children) return null;
  return <p className="mt-3 max-w-[80ch] text-xs leading-relaxed text-ink-3">{children}</p>;
}

type Tone = "ok" | "bad" | "unsure";

const TONES: Record<Tone, { box: string; icon: typeof CircleCheck; iconClass: string }> = {
  ok: { box: "bg-accent-tint", icon: CircleCheck, iconClass: "text-accent" },
  bad: { box: "bg-danger-tint", icon: CircleAlert, iconClass: "text-danger" },
  unsure: { box: "bg-warning-tint", icon: CircleHelp, iconClass: "text-warning" },
};

/** A verdict word with what it does and does not mean. */
export function Verdict({ tone, word, children, aside }: { tone: Tone; word: string; children?: ReactNode; aside?: ReactNode }) {
  const t = TONES[tone];
  const Icon = t.icon;
  return (
    <div role={tone === "bad" ? "alert" : "status"} className={cx("flex flex-wrap items-start gap-3 rounded-card p-4", t.box)}>
      <Icon className={cx("mt-0.5 size-5 shrink-0", t.iconClass)} aria-hidden />
      <div className="min-w-0 flex-1">
        <p className="font-mono text-[15px] font-semibold tracking-tight text-ink">{word}</p>
        {children ? <div className="mt-0.5 text-[13px] text-ink-2">{children}</div> : null}
      </div>
      {aside}
    </div>
  );
}

/** A list the reader must not skip: what a run left out. */
export function NotDone({ title, items, children }: { title: string; items: string[]; children?: ReactNode }) {
  if (!items.length) return null;
  return (
    <div className="rounded-card bg-warning-tint p-3">
      <p className="flex items-center gap-1.5 text-[13px] font-medium text-ink">
        <TriangleAlert className="size-4 text-warning" aria-hidden /> {title}
      </p>
      {children ? <p className="mt-1 text-[13px] text-ink-2">{children}</p> : null}
      <ul className="mt-2 flex flex-wrap gap-1.5">
        {items.map((i) => (
          <li key={i} className="rounded bg-raised px-1.5 py-0.5 font-mono text-xs text-ink-2 shadow-card">
            {i}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** Save text as a file in the browser. */
export function saveText(name: string, text: string, type = "text/plain") {
  const url = URL.createObjectURL(new Blob([text], { type: `${type};charset=utf-8` }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function DownloadButton({ name, text, type }: { name: string; text: string; type?: string }) {
  return (
    <Button size="sm" icon={<Download className="size-3.5" aria-hidden />} onClick={() => saveText(name, text, type)}>
      <span className="font-mono text-[12px]">{name}</span>
    </Button>
  );
}

/** Parse a JSON value typed by a person: numbers, booleans, null and quoted strings as JSON, anything else as text. */
export function looseValue(raw: string): string | number | boolean | null {
  const t = raw.trim();
  if (/^(-?\d+(\.\d+)?|true|false|null|"[^"]*")$/.test(t)) return JSON.parse(t) as string | number | boolean | null;
  return t;
}

export const SEVERITY_DOT: Record<string, string> = {
  error: "var(--danger)",
  violation: "var(--danger)",
  warning: "var(--warning)",
  info: "var(--text-3)",
};
