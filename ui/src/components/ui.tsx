import { forwardRef, useEffect, useId, useRef, type ButtonHTMLAttributes, type InputHTMLAttributes, type ReactNode, type SelectHTMLAttributes, type TextareaHTMLAttributes } from "react";
import { LoaderCircle, X } from "lucide-react";

export function cx(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}

type Variant = "primary" | "secondary" | "ghost" | "danger";
type Size = "sm" | "md";

const variants: Record<Variant, string> = {
  primary: "bg-accent text-on-accent hover:bg-accent-hover shadow-card",
  secondary: "bg-raised text-ink shadow-card hover:bg-sunken",
  ghost: "text-ink-2 hover:bg-sunken hover:text-ink",
  danger: "bg-raised text-danger shadow-card hover:bg-danger-tint",
};

const sizes: Record<Size, string> = {
  sm: "h-8 px-2.5 gap-1.5 text-[13px]",
  md: "h-9 px-3.5 gap-2 text-sm",
};

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  busy?: boolean;
  icon?: ReactNode;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { variant = "secondary", size = "md", busy, icon, className, children, disabled, ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      disabled={disabled || busy}
      className={cx(
        "inline-flex shrink-0 items-center justify-center rounded-input font-medium whitespace-nowrap select-none",
        "transition-[background-color,color,box-shadow] duration-150 touch-manipulation",
        "disabled:cursor-not-allowed disabled:opacity-50",
        variants[variant],
        sizes[size],
        className,
      )}
      {...rest}
    >
      {busy ? <LoaderCircle className="size-4 animate-spin" aria-hidden /> : icon}
      {children}
    </button>
  );
});

export function IconButton({ label, className, ...rest }: ButtonHTMLAttributes<HTMLButtonElement> & { label: string }) {
  return (
    <button
      aria-label={label}
      title={label}
      className={cx(
        "inline-flex size-8 shrink-0 items-center justify-center rounded-input text-ink-2 hover:bg-sunken hover:text-ink",
        "transition-colors duration-150 disabled:opacity-40",
        className,
      )}
      {...rest}
    />
  );
}

const field =
  "w-full rounded-input bg-raised px-2.5 text-sm text-ink shadow-card placeholder:text-ink-3 outline-none " +
  "focus-visible:outline-2 focus-visible:outline-accent disabled:opacity-60";

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(function Input(
  { className, ...rest },
  ref,
) {
  return <input ref={ref} className={cx(field, "h-9", className)} {...rest} />;
});

export function Textarea({ className, ...rest }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea className={cx(field, "min-h-20 py-2 leading-relaxed", className)} {...rest} />;
}

export function Select({ className, children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select className={cx(field, "h-9 pr-7", className)} {...rest}>
      {children}
    </select>
  );
}

export function Field({ label, hint, children, error }: { label: string; hint?: string; error?: string; children: (id: string) => ReactNode }) {
  const id = useId();
  return (
    <div className="flex flex-col gap-1.5">
      <label htmlFor={id} className="text-[13px] font-medium text-ink">
        {label}
      </label>
      {children(id)}
      {error ? <p className="text-xs text-danger">{error}</p> : hint ? <p className="text-xs text-ink-3">{hint}</p> : null}
    </div>
  );
}

export function Checkbox({ label, checked, onChange, disabled }: { label: string; checked: boolean; onChange: (v: boolean) => void; disabled?: boolean }) {
  return (
    <label className={cx("inline-flex items-center gap-2 text-[13px] text-ink select-none", disabled && "opacity-50")}>
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        className="size-4 rounded accent-[var(--accent)]"
      />
      {label}
    </label>
  );
}

export function Badge({ children, tone = "neutral", className }: { children: ReactNode; tone?: "neutral" | "danger" | "warning" | "accent"; className?: string }) {
  const tones = {
    neutral: "bg-sunken text-ink-2",
    danger: "bg-danger-tint text-danger",
    warning: "bg-warning-tint text-[color-mix(in_oklch,var(--warning)_75%,var(--text))]",
    accent: "bg-accent-tint text-accent",
  };
  return <span className={cx("inline-flex h-5 items-center rounded px-1.5 text-xs font-medium", tones[tone], className)}>{children}</span>;
}

export function Dot({ color, className }: { color: string; className?: string }) {
  return <span aria-hidden className={cx("inline-block size-2 shrink-0 rounded-full", className)} style={{ background: color }} />;
}

export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="rounded border border-hairline bg-sunken px-1 font-mono text-[11px] text-ink-2">{children}</kbd>;
}

export function Spinner({ label = "Loading" }: { label?: string }) {
  return (
    <span role="status" className="inline-flex items-center gap-2 text-sm text-ink-3">
      <LoaderCircle className="size-4 animate-spin" aria-hidden />
      {label}
    </span>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={cx("animate-pulse rounded bg-sunken", className)} />;
}

export function EmptyState({ title, children, action }: { title: string; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 px-6 py-14 text-center">
      <EmptyMark />
      <div className="max-w-sm">
        <p className="font-medium text-ink">{title}</p>
        {children ? <div className="mt-1 text-sm text-ink-2">{children}</div> : null}
      </div>
      {action}
    </div>
  );
}

/** Three unlinked nodes: the empty map waiting for its first relation. */
function EmptyMark() {
  return (
    <svg width="72" height="44" viewBox="0 0 72 44" aria-hidden className="text-ink-3">
      <path d="M14 30 36 12" stroke="currentColor" strokeWidth="1.5" strokeDasharray="3 4" opacity=".6" />
      <path d="M36 12 58 30" stroke="currentColor" strokeWidth="1.5" strokeDasharray="3 4" opacity=".35" />
      <circle cx="14" cy="30" r="6" fill="none" stroke="currentColor" strokeWidth="1.5" />
      <circle cx="36" cy="12" r="7" fill="var(--accent-tint)" stroke="var(--accent)" strokeWidth="1.5" />
      <circle cx="58" cy="30" r="6" fill="none" stroke="currentColor" strokeWidth="1.5" />
    </svg>
  );
}

export function ErrorState({ error, retry }: { error: unknown; retry?: () => void }) {
  const msg = error instanceof Error ? error.message : String(error);
  return (
    <div role="alert" className="flex flex-col items-start gap-2 rounded-card bg-danger-tint p-4 text-sm text-ink">
      <p className="font-medium text-danger">That did not load</p>
      <p className="text-ink-2">{msg}</p>
      {retry ? (
        <Button size="sm" onClick={retry}>
          Try again
        </Button>
      ) : null}
    </div>
  );
}

export function Segmented<T extends string>({ value, options, onChange, label }: { value: T; options: { value: T; label: string; icon?: ReactNode }[]; onChange: (v: T) => void; label: string }) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex rounded-input bg-sunken p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          role="radio"
          aria-checked={value === o.value}
          onClick={() => onChange(o.value)}
          className={cx(
            "inline-flex h-7 items-center gap-1.5 rounded-[5px] px-2.5 text-[13px] font-medium transition-[background-color,color,box-shadow] duration-150",
            value === o.value ? "bg-raised text-ink shadow-card" : "text-ink-2 hover:text-ink",
          )}
        >
          {o.icon}
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** Side panel on wide screens, bottom sheet on phones. */
export function Panel({ open, onClose, title, children, actions, wide }: { open: boolean; onClose: () => void; title: ReactNode; children: ReactNode; actions?: ReactNode; wide?: boolean }) {
  if (!open) return null;
  return (
    <aside
      className={cx(
        "flex min-h-0 flex-col bg-raised shadow-overlay md:shadow-[-1px_0_0_var(--hairline)]",
        "fixed inset-x-0 bottom-14 z-30 max-h-[60dvh] rounded-t-modal md:static md:z-auto md:max-h-none md:rounded-none",
        wide ? "md:w-[440px]" : "md:w-[360px]",
      )}
    >
      <div className="mx-auto mt-2 h-1 w-10 shrink-0 rounded-full bg-hairline md:hidden" aria-hidden />
      <div className="flex items-start gap-2 border-b border-hairline px-4 py-3">
        <div className="min-w-0 flex-1">{title}</div>
        {actions}
        <IconButton label="Close" onClick={onClose}>
          <X className="size-4" />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain px-4 pb-6">{children}</div>
    </aside>
  );
}

/** Modal dialog on the native <dialog> element (focus trap, Esc, backdrop). */
export function Dialog({ open, onClose, title, children, footer, wide }: { open: boolean; onClose: () => void; title: string; children: ReactNode; footer?: ReactNode; wide?: boolean }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  return (
    <dialog
      ref={ref}
      onClose={onClose}
      onClick={(e) => e.target === ref.current && onClose()}
      className={cx(
        "m-auto max-h-[88dvh] w-[calc(100vw-24px)] overflow-hidden rounded-modal bg-raised p-0 text-ink shadow-overlay backdrop:bg-black/30",
        wide ? "max-w-3xl" : "max-w-lg",
      )}
    >
      {open ? (
        <div className="flex max-h-[88dvh] flex-col">
          <div className="flex items-center gap-2 border-b border-hairline px-5 py-3.5">
            <h2 className="flex-1 text-[15px] font-semibold tracking-tight">{title}</h2>
            <IconButton label="Close" onClick={onClose}>
              <X className="size-4" />
            </IconButton>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">{children}</div>
          {footer ? <div className="flex flex-wrap justify-end gap-2 border-t border-hairline px-5 py-3">{footer}</div> : null}
        </div>
      ) : null}
    </dialog>
  );
}

export function Section({ title, children, aside }: { title: string; children: ReactNode; aside?: ReactNode }) {
  return (
    <section className="mt-5">
      <div className="mb-2 flex items-center gap-2">
        <h3 className="flex-1 text-[13px] font-semibold text-ink">{title}</h3>
        {aside}
      </div>
      {children}
    </section>
  );
}

/** Editable list of short strings (states, kinds, roles). */
export function TagInput({ value, onChange, placeholder, suggestions }: { value: string[]; onChange: (v: string[]) => void; placeholder?: string; suggestions?: string[] }) {
  const listId = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const add = (raw: string) => {
    const v = raw.trim();
    if (v && !value.includes(v)) onChange([...value, v]);
  };
  return (
    <div className="flex min-h-9 flex-wrap items-center gap-1 rounded-input bg-raised px-1.5 py-1 shadow-card focus-within:outline-2 focus-within:outline-accent" onClick={() => inputRef.current?.focus()}>
      {value.map((t) => (
        <span key={t} className="inline-flex h-6 items-center gap-1 rounded bg-sunken pr-0.5 pl-1.5 font-mono text-xs text-ink">
          {t}
          <button aria-label={`Remove ${t}`} className="rounded p-0.5 text-ink-3 hover:text-ink" onClick={() => onChange(value.filter((x) => x !== t))}>
            <X className="size-3" />
          </button>
        </span>
      ))}
      <input
        ref={inputRef}
        list={suggestions ? listId : undefined}
        placeholder={value.length ? "" : placeholder}
        className="h-6 min-w-24 flex-1 bg-transparent px-1 text-sm outline-none placeholder:text-ink-3"
        onKeyDown={(e) => {
          const t = e.currentTarget;
          if ((e.key === "Enter" || e.key === ",") && t.value) {
            e.preventDefault();
            add(t.value);
            t.value = "";
          } else if (e.key === "Backspace" && !t.value && value.length) {
            onChange(value.slice(0, -1));
          }
        }}
        onBlur={(e) => {
          if (e.currentTarget.value) {
            add(e.currentTarget.value);
            e.currentTarget.value = "";
          }
        }}
      />
      {suggestions ? (
        <datalist id={listId}>
          {suggestions.map((s) => (
            <option key={s} value={s} />
          ))}
        </datalist>
      ) : null}
    </div>
  );
}
