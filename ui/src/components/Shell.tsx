import { useEffect, useState, type ReactNode } from "react";
import { Blocks, Boxes, GitPullRequestArrow, LogOut, Map as MapIcon, Search, Shapes, Table2 } from "lucide-react";

import { useOpenProposals, useWhoami } from "../lib/queries";
import { href, useRoute } from "../lib/router";
import { clearToken, useSession } from "../lib/session";
import { CommandPalette } from "./CommandPalette";
import { ConsoleShell } from "./ConsoleShell";
import { Kbd, cx } from "./ui";

export const NAV = [
  { key: "map", label: "Map", short: "Map", icon: MapIcon },
  { key: "explore", label: "Explorer", short: "Explore", icon: Table2 },
  { key: "entities", label: "Entities", short: "Entities", icon: Boxes },
  { key: "schema", label: "Schema", short: "Schema", icon: Shapes },
  { key: "build", label: "Builder", short: "Build", icon: Blocks },
  { key: "proposals", label: "Proposals", short: "Proposals", icon: GitPullRequestArrow },
] as const;

/** The app frame. With a console session it adds the project switcher and the Project group (ConsoleShell). */
export function Shell({ children, title, embedded, consoleMode }: { children: ReactNode; title: string; embedded: boolean; consoleMode?: boolean }) {
  if (consoleMode) return <ConsoleShell title={title}>{children}</ConsoleShell>;
  return (
    <OperatorShell title={title} embedded={embedded}>
      {children}
    </OperatorShell>
  );
}

/** Shortcuts every shell shares: ⌘K and / open the palette (unless a page took the key). */
export function usePaletteKeys(setPalette: (f: (p: boolean) => boolean) => void) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPalette((p) => !p);
      } else if (e.key === "/" && !isTyping(e.target)) {
        e.preventDefault();
        setPalette(() => true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [setPalette]);
}

export function isTyping(t: EventTarget | null): boolean {
  return t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t instanceof HTMLSelectElement || (t instanceof HTMLElement && t.isContentEditable);
}

function OperatorShell({ children, title, embedded }: { children: ReactNode; title: string; embedded: boolean }) {
  const route = useRoute();
  const me = useWhoami().data;
  const session = useSession();
  const open = useOpenProposals().data?.proposals.length ?? 0;
  const [palette, setPalette] = useState(false);
  usePaletteKeys(setPalette);

  const active = route.path[0];
  const keep = { workspace: session.workspace };

  return (
    <div className="flex h-dvh flex-col md:flex-row">
      <nav aria-label="Main" className="hidden w-14 shrink-0 flex-col bg-sunken shadow-[1px_0_0_var(--hairline)] md:flex lg:w-[220px]">
        <div className={cx("flex h-14 items-center gap-2.5 px-4", embedded && "lg:h-12")}>
          <Logo />
          <span className="hidden truncate text-[14px] font-semibold tracking-tight lg:inline">{title}</span>
        </div>
        <button
          onClick={() => setPalette(true)}
          className="mx-2 mb-2 flex h-8 items-center gap-2 rounded-input bg-raised px-2.5 text-[13px] text-ink-3 shadow-card hover:text-ink-2"
        >
          <Search className="size-4 shrink-0" aria-hidden />
          <span className="hidden flex-1 text-left lg:inline">Jump to…</span>
          <span className="hidden lg:inline">
            <Kbd>⌘K</Kbd>
          </span>
        </button>
        <ul className="flex flex-col gap-0.5 px-2">
          {NAV.map(({ key, label, icon: Icon }) => (
            <li key={key}>
              <a
                href={href([key], keep)}
                aria-current={active === key ? "page" : undefined}
                className={cx(
                  "relative flex h-8 items-center gap-2.5 rounded-input px-2.5 text-[13px] font-medium transition-colors duration-150",
                  active === key ? "bg-accent-tint text-accent" : "text-ink-2 hover:bg-raised hover:text-ink",
                )}
              >
                <Icon className="size-4 shrink-0" aria-hidden />
                <span className="hidden flex-1 lg:inline">{label}</span>
                {key === "proposals" && open > 0 ? (
                  <span className="tnum hidden min-w-5 rounded-full bg-raised px-1.5 text-center text-[11px] text-ink-2 shadow-card lg:inline" aria-label={`${open} open`}>
                    {open}
                  </span>
                ) : null}
              </a>
            </li>
          ))}
        </ul>
        <div className="mt-auto hidden border-t border-hairline px-4 py-3 lg:block">
          {me ? (
            <>
              <p className="truncate text-[13px] font-medium text-ink" title={me.principal.id}>
                {me.principal.id}
              </p>
              <p className="truncate text-xs text-ink-3">
                {me.principal.roles.join(", ") || "no roles"} · {me.tenant}
                {me.principal.via ? ` · via ${me.principal.via}` : ""}
              </p>
            </>
          ) : null}
          {!embedded && session.token ? (
            <button onClick={clearToken} className="mt-2 inline-flex items-center gap-1.5 text-xs text-ink-3 hover:text-ink">
              <LogOut className="size-3.5" aria-hidden /> Sign out
            </button>
          ) : null}
        </div>
      </nav>

      <main className="flex min-h-0 min-w-0 flex-1 flex-col pb-14 md:pb-0">{children}</main>

      <nav aria-label="Main" className="fixed inset-x-0 bottom-0 z-20 flex h-14 bg-raised pb-[env(safe-area-inset-bottom)] shadow-[0_-1px_0_var(--hairline)] md:hidden">
        {NAV.map(({ key, short, label, icon: Icon }) => (
          <a
            key={key}
            href={href([key], keep)}
            aria-current={active === key ? "page" : undefined}
            aria-label={label}
            className={cx("flex min-w-0 flex-1 flex-col items-center justify-center gap-0.5 px-0.5 text-[10.5px] font-medium", active === key ? "text-accent" : "text-ink-3")}
          >
            <Icon className="size-5 shrink-0" aria-hidden />
            <span className="max-w-full truncate">{short}</span>
          </a>
        ))}
        <button onClick={() => setPalette(true)} className="flex min-w-0 flex-1 flex-col items-center justify-center gap-0.5 px-0.5 text-[10.5px] font-medium text-ink-3">
          <Search className="size-5" aria-hidden />
          Search
        </button>
      </nav>

      <CommandPalette open={palette} onClose={() => setPalette(false)} />
    </div>
  );
}

export function Logo() {
  return (
    <svg width="22" height="22" viewBox="0 0 32 32" aria-hidden className="shrink-0">
      <rect width="32" height="32" rx="8" fill="var(--text)" />
      <g fill="none" stroke="var(--accent)" strokeWidth="2.2" strokeLinecap="round">
        <path d="M10 21 16 11l6 10" />
        <path d="M10 21h12" />
      </g>
      <g fill="var(--page)">
        <circle cx="16" cy="11" r="3" />
        <circle cx="10" cy="21" r="3" />
        <circle cx="22" cy="21" r="3" />
      </g>
    </svg>
  );
}

/** Page header: title, context, actions; wraps on phones. */
export function PageHeader({ title, context, children }: { title: string; context?: ReactNode; children?: ReactNode }) {
  return (
    <header className="flex min-h-14 flex-wrap items-center gap-x-3 gap-y-2 border-b border-hairline px-4 py-2.5 md:px-5">
      <div className="min-w-0 flex-1">
        <h1 className="truncate text-[15px] font-semibold tracking-tight">{title}</h1>
        {context ? <p className="truncate text-xs text-ink-3">{context}</p> : null}
      </div>
      {children ? <div className="flex flex-wrap items-center gap-2">{children}</div> : null}
    </header>
  );
}
