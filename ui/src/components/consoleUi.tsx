// Building blocks of the console pages, on top of components/ui.tsx.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Check, Copy, TriangleAlert } from "lucide-react";

import { atLeast, needs, type OrgRole } from "../lib/console";
import { Button, Dialog, Field, Input, cx, type ButtonProps } from "./ui";

/** A page body: 16 px gutter on phones, 20 px from md, scrolls on its own. */
export function Page({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx("min-h-0 flex-1 overflow-y-auto px-4 pt-4 pb-10 md:px-5 md:pt-5", className)}>{children}</div>;
}

export function Card({ title, aside, children, className, pad = true }: { title?: ReactNode; aside?: ReactNode; children: ReactNode; className?: string; pad?: boolean }) {
  return (
    <section className={cx("min-w-0 rounded-card bg-raised shadow-card", pad && "p-4", className)}>
      {title ? (
        <div className={cx("mb-3 flex min-h-6 items-center gap-2", !pad && "px-4 pt-4")}>
          <h2 className="flex-1 truncate text-[13px] font-semibold text-ink">{title}</h2>
          {aside}
        </div>
      ) : null}
      {children}
    </section>
  );
}

/**
 * A button for a role-limited action. Without the role it is disabled and
 * names the role it needs (the server refuses anyway).
 */
export function RoleButton({ role, min, children, title, ...rest }: ButtonProps & { role: OrgRole | undefined; min: OrgRole }) {
  if (atLeast(role, min)) {
    return (
      <Button title={title} {...rest}>
        {children}
      </Button>
    );
  }
  const why = needs(min);
  return (
    <span title={why} className="inline-flex">
      <Button {...rest} disabled aria-disabled onClick={undefined}>
        {children}
        <span className="sr-only">({why})</span>
      </Button>
    </span>
  );
}

export function useCopy(): [boolean, (text: string) => void] {
  const [done, setDone] = useState(false);
  const t = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(t.current), []);
  return [
    done,
    (text: string) => {
      const ok = () => {
        setDone(true);
        window.clearTimeout(t.current);
        t.current = window.setTimeout(() => setDone(false), 1500);
      };
      if (navigator.clipboard?.writeText) {
        navigator.clipboard.writeText(text).then(ok, () => fallbackCopy(text) && ok());
      } else if (fallbackCopy(text)) {
        ok();
      }
    },
  ];
}

function fallbackCopy(text: string): boolean {
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  ta.remove();
  return ok;
}

export function CopyButton({ text, label = "Copy", className }: { text: string; label?: string; className?: string }) {
  const [done, copy] = useCopy();
  return (
    <Button
      size="sm"
      variant="ghost"
      className={className}
      onClick={() => copy(text)}
      icon={done ? <Check className="size-3.5" aria-hidden /> : <Copy className="size-3.5" aria-hidden />}
    >
      <span aria-live="polite">{done ? "Copied" : label}</span>
    </Button>
  );
}

/** A code snippet with a copy button; long lines scroll inside the block. */
export function CodeBlock({ code, label, className }: { code: string; label?: string; className?: string }) {
  return (
    <div className={cx("min-w-0 overflow-hidden rounded-card bg-sunken shadow-card", className)}>
      <div className="flex items-center gap-2 border-b border-hairline py-1 pr-1 pl-3">
        <span className="flex-1 truncate text-xs text-ink-3">{label}</span>
        <CopyButton text={code} />
      </div>
      <pre className="overflow-x-auto overscroll-x-contain px-3 py-2.5 font-mono text-[12.5px] leading-relaxed text-ink">
        <code>{code}</code>
      </pre>
    </div>
  );
}

/** One metric: label, value, comparison in plain secondary text, optional sparkline. */
export function Stat({
  label,
  value,
  sub,
  tone,
  primary,
  children,
}: {
  label: string;
  value: ReactNode;
  sub?: string;
  tone?: "good" | "bad" | "flat";
  primary?: boolean;
  children?: ReactNode;
}) {
  return (
    <div className={cx("min-w-0 rounded-card p-4 shadow-card", primary ? "bg-accent-tint" : "bg-raised")}>
      <p className="truncate text-[13px] text-ink-2">{label}</p>
      <p className={cx("tnum mt-1 truncate font-semibold tracking-tight text-ink", primary ? "text-[30px] leading-9" : "text-[22px] leading-7")}>{value}</p>
      {sub ? <p className={cx("mt-0.5 truncate text-xs", tone === "bad" ? "text-danger" : "text-ink-3")}>{sub}</p> : null}
      {children ? <div className="mt-2">{children}</div> : null}
    </div>
  );
}

/** Tabs as a tablist with arrow-key movement. */
export function Tabs<T extends string>({ value, onChange, tabs, label }: { value: T; onChange: (v: T) => void; tabs: { value: T; label: string }[]; label: string }) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  return (
    <div role="tablist" aria-label={label} className="-mx-1 flex gap-1 overflow-x-auto border-b border-hairline px-1">
      {tabs.map((t, i) => (
        <button
          key={t.value}
          ref={(el) => {
            refs.current[i] = el;
          }}
          role="tab"
          aria-selected={value === t.value}
          tabIndex={value === t.value ? 0 : -1}
          onClick={() => onChange(t.value)}
          onKeyDown={(e) => {
            if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
            e.preventDefault();
            const j = (i + (e.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length;
            onChange(tabs[j].value);
            refs.current[j]?.focus();
          }}
          className={cx(
            "relative h-9 shrink-0 px-2.5 text-[13px] font-medium whitespace-nowrap transition-colors duration-150",
            value === t.value ? "text-ink after:absolute after:inset-x-2 after:-bottom-px after:h-0.5 after:rounded-full after:bg-accent" : "text-ink-3 hover:text-ink",
          )}
        >
          {t.label}
        </button>
      ))}
    </div>
  );
}

/** A destructive confirmation; with `type`, the user must type that text first. */
export function ConfirmDialog({
  open,
  onClose,
  title,
  children,
  confirm,
  onConfirm,
  busy,
  type,
  error,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  children: ReactNode;
  confirm: string;
  onConfirm: () => void;
  busy?: boolean;
  type?: string;
  error?: string;
}) {
  const [typed, setTyped] = useState("");
  useEffect(() => {
    if (open) setTyped("");
  }, [open]);
  const ready = !type || typed === type;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={title}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="danger" busy={busy} disabled={!ready} onClick={onConfirm}>
            {confirm}
          </Button>
        </>
      }
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (ready) onConfirm();
        }}
        className="flex flex-col gap-3 text-sm text-ink-2"
      >
        {children}
        {type ? (
          <Field label={`Type ${type} to confirm`}>
            {(id) => <Input id={id} value={typed} onChange={(e) => setTyped(e.target.value)} autoComplete="off" autoFocus spellCheck={false} />}
          </Field>
        ) : null}
        {error ? <p className="text-danger">{error}</p> : null}
      </form>
    </Dialog>
  );
}

/** The one-time view of a new API key's secret. */
export function SecretReveal({ secret, note }: { secret: string; note?: string }) {
  return (
    <div className="flex flex-col gap-3">
      <div role="alert" className="flex gap-2 rounded-card bg-warning-tint p-3 text-[13px] text-ink">
        <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
        <p>Copy this key now. It is shown once and cannot be shown again; only its prefix is kept.{note ? ` ${note}` : ""}</p>
      </div>
      <CodeBlock code={secret} label="Secret key" />
    </div>
  );
}

/** Status dot with a word. */
export function Status({ tone, children }: { tone: "ok" | "error" | "warn" | "muted"; children: ReactNode }) {
  const color = { ok: "var(--success)", error: "var(--danger)", warn: "var(--warning)", muted: "var(--text-3)" }[tone];
  return (
    <span className="inline-flex items-center gap-1.5 text-[13px] text-ink-2">
      <span aria-hidden className="inline-block size-1.5 rounded-full" style={{ background: color }} />
      {children}
    </span>
  );
}

export const th = "h-9 px-3 text-left text-xs font-medium whitespace-nowrap text-ink-3";
export const td = "px-3 py-2.5 align-middle";
