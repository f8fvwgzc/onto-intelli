// The shell with a console session (docs/CONTROL_PLANE.md section 11): the
// organization and project switcher on top of the sidebar (in the header on
// phones), then the Project and Ontology groups, with Members and the user
// menu in the footer.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Activity, Cable, ChartNoAxesColumn, Cpu, KeyRound, LayoutDashboard, LogOut, LayoutGrid, Menu, MessagesSquare, Search, Settings, Users } from "lucide-react";

import { consoleApi, useConsoleScope } from "../lib/console";
import { useOpenProposals } from "../lib/queries";
import { href, navigate, useRoute } from "../lib/router";
import { clearToken } from "../lib/session";
import { markSignedOut } from "../pages/console/SignInPage";
import { CommandPalette } from "./CommandPalette";
import { ProjectSwitcher } from "./ProjectSwitcher";
import { Logo, NAV, usePaletteKeys } from "./Shell";
import { Dialog, Kbd, cx } from "./ui";

export const PROJECT_NAV = [
  { key: "overview", label: "Overview", icon: LayoutDashboard },
  { key: "traces", label: "Traces", icon: Activity },
  { key: "sessions", label: "Sessions", icon: MessagesSquare },
  { key: "usage", label: "Usage", icon: ChartNoAxesColumn },
  { key: "connect", label: "Connect", icon: Cable },
  { key: "keys", label: "API keys", icon: KeyRound },
  { key: "llm", label: "LLM", icon: Cpu },
  { key: "settings", label: "Settings", icon: Settings },
] as const;

type Item = { key: string; label: string; icon: React.ComponentType<{ className?: string; "aria-hidden"?: boolean }> };

/** The organization's pages (sidebar footer, phone sheet). */
const ORG_NAV = [
  { key: "members", label: "Members", icon: Users },
  { key: "projects", label: "All projects", icon: LayoutGrid },
] as const;

/** Console pages the ⌘K palette lists, besides the ontology views. */
const PALETTE_VIEWS = [...PROJECT_NAV, ...ORG_NAV];

function NavLink({ item, active, badge, onClick, rail }: { item: Item; active: boolean; badge?: number; onClick?: () => void; rail?: boolean }) {
  const Icon = item.icon;
  return (
    <a
      href={href([item.key])}
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      title={rail ? item.label : undefined}
      className={cx(
        "relative flex h-8 items-center gap-2.5 rounded-input px-2.5 text-[13px] font-medium transition-colors duration-150",
        active ? "bg-accent-tint text-accent" : "text-ink-2 hover:bg-raised hover:text-ink",
      )}
    >
      <Icon className="size-4 shrink-0" aria-hidden />
      <span className={cx("flex-1 truncate", rail && "hidden lg:inline")}>{item.label}</span>
      {badge ? (
        <span className={cx("tnum min-w-5 rounded-full bg-raised px-1.5 text-center text-[11px] text-ink-2 shadow-card", rail && "hidden lg:inline")} aria-label={`${badge} open`}>
          {badge}
        </span>
      ) : null}
    </a>
  );
}

function Group({
  label,
  items,
  active,
  rail,
  onNavigate,
  badges,
}: {
  label: string;
  items: readonly Item[];
  active: string;
  rail?: boolean;
  onNavigate?: () => void;
  badges?: Record<string, number>;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <p className={cx("px-2.5 pt-3 pb-1 text-[11.5px] font-medium text-ink-3", rail && "hidden lg:block")}>{label}</p>
      {rail ? <div className="mx-3 my-2 h-px bg-hairline lg:hidden" aria-hidden /> : null}
      <ul className="flex flex-col gap-0.5">
        {items.map((it) => (
          <li key={it.key}>
            <NavLink item={it} active={active === it.key} rail={rail} onClick={onNavigate} badge={badges?.[it.key]} />
          </li>
        ))}
      </ul>
    </div>
  );
}

function useSignOut() {
  const qc = useQueryClient();
  return async (everywhere: boolean) => {
    try {
      await consoleApi.logout(everywhere);
    } catch {
      // The token is dropped either way.
    }
    markSignedOut(true);
    clearToken();
    qc.clear();
    navigate(["signin"]);
  };
}

function UserMenu({ rail }: { rail?: boolean }) {
  const { me } = useConsoleScope();
  const signOut = useSignOut();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => !ref.current?.contains(e.target as Node) && setOpen(false);
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("pointerdown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);
  if (!me) return null;
  const name = me.user.name || me.user.email || "Signed in";
  return (
    <div ref={ref} className="relative">
      <button
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        title={rail ? name : undefined}
        className="flex h-10 w-full min-w-0 items-center gap-2 rounded-input px-2 text-left hover:bg-raised"
      >
        <span aria-hidden className="grid size-6 shrink-0 place-items-center rounded-full bg-gray-3 text-[11px] font-semibold text-ink">
          {name.slice(0, 1).toUpperCase()}
        </span>
        <span className={cx("min-w-0 flex-1", rail && "hidden lg:block")}>
          <span className="block truncate text-[13px] font-medium text-ink">{name}</span>
          {me.user.email && me.user.email !== name ? <span className="block truncate text-[11.5px] text-ink-3">{me.user.email}</span> : null}
        </span>
      </button>
      {open ? (
        <div role="menu" className={cx("absolute bottom-full z-40 mb-1 w-56 rounded-card bg-raised p-1 shadow-overlay", rail ? "left-0" : "inset-x-0 w-auto")}>
          <button
            role="menuitem"
            autoFocus
            onClick={() => void signOut(false)}
            className="flex h-8 w-full items-center gap-2 rounded-input px-2 text-left text-[13px] text-ink hover:bg-sunken focus-visible:bg-sunken"
          >
            <LogOut className="size-4" aria-hidden /> Sign out
          </button>
          <button
            role="menuitem"
            onClick={() => void signOut(true)}
            className="flex h-8 w-full items-center gap-2 rounded-input px-2 text-left text-[13px] text-ink hover:bg-sunken focus-visible:bg-sunken"
          >
            <LogOut className="size-4 text-danger" aria-hidden /> Sign out everywhere
          </button>
        </div>
      ) : null}
    </div>
  );
}

export function ConsoleShell({ children, title }: { children: ReactNode; title: string }) {
  const route = useRoute();
  const open = useOpenProposals().data?.proposals.length ?? 0;
  const [palette, setPalette] = useState(false);
  const [sheet, setSheet] = useState(false);
  usePaletteKeys(setPalette);
  const active = route.path[0] === "trace" ? "traces" : route.path[0];
  const badges = { proposals: open };

  return (
    <div className="flex h-dvh flex-col md:flex-row">
      <nav aria-label="Main" className="hidden w-14 shrink-0 flex-col bg-sunken shadow-[1px_0_0_var(--hairline)] md:flex lg:w-[220px]">
        <div className="flex h-12 items-center gap-2.5 px-4">
          <Logo />
          <span className="hidden truncate text-[14px] font-semibold tracking-tight lg:inline">{title}</span>
        </div>
        <div className="px-2 pb-1">
          <div className="hidden lg:block">
            <ProjectSwitcher />
          </div>
          <div className="flex justify-center lg:hidden">
            <ProjectSwitcher rail />
          </div>
        </div>
        <button
          onClick={() => setPalette(true)}
          aria-label="Jump to"
          className="mx-2 mt-1 flex h-8 items-center gap-2 rounded-input bg-raised px-2.5 text-[13px] text-ink-3 shadow-card hover:text-ink-2"
        >
          <Search className="size-4 shrink-0" aria-hidden />
          <span className="hidden flex-1 text-left lg:inline">Jump to…</span>
          <span className="hidden lg:inline">
            <Kbd>⌘K</Kbd>
          </span>
        </button>
        <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
          <Group label="Project" items={PROJECT_NAV} active={active} rail />
          <Group label="Ontology" items={NAV} active={active} rail badges={badges} />
        </div>
        <div className="flex flex-col gap-0.5 border-t border-hairline px-2 py-2">
          <NavLink item={{ key: "members", label: "Members", icon: Users }} active={active === "members"} rail />
          <UserMenu rail />
        </div>
      </nav>

      <header className="flex h-12 shrink-0 items-center gap-2 bg-sunken px-4 shadow-[0_1px_0_var(--hairline)] md:hidden">
        <Logo />
        <div className="min-w-0 flex-1">
          <ProjectSwitcher compact />
        </div>
        <button onClick={() => setPalette(true)} aria-label="Jump to" className="grid size-9 place-items-center rounded-input text-ink-2 hover:bg-raised">
          <Search className="size-5" aria-hidden />
        </button>
      </header>

      <main className="flex min-h-0 min-w-0 flex-1 flex-col pb-14 md:pb-0">{children}</main>

      <nav aria-label="Main" className="fixed inset-x-0 bottom-0 z-20 flex h-14 bg-raised pb-[env(safe-area-inset-bottom)] shadow-[0_-1px_0_var(--hairline)] md:hidden">
        {[PROJECT_NAV[0], PROJECT_NAV[1], NAV[0], NAV[1]].map(({ key, label, icon: Icon }) => (
          <a
            key={key}
            href={href([key])}
            aria-current={active === key ? "page" : undefined}
            className={cx("flex min-w-0 flex-1 flex-col items-center justify-center gap-0.5 px-0.5 text-[10.5px] font-medium", active === key ? "text-accent" : "text-ink-3")}
          >
            <Icon className="size-5 shrink-0" aria-hidden />
            <span className="max-w-full truncate">{label}</span>
          </a>
        ))}
        <button
          onClick={() => setSheet(true)}
          aria-haspopup="dialog"
          className="flex min-w-0 flex-1 flex-col items-center justify-center gap-0.5 px-0.5 text-[10.5px] font-medium text-ink-3"
        >
          <Menu className="size-5" aria-hidden />
          More
        </button>
      </nav>

      <Dialog open={sheet} onClose={() => setSheet(false)} title="Go to">
        <div className="-mt-2 pb-2">
          <Group label="Project" items={PROJECT_NAV} active={active} onNavigate={() => setSheet(false)} />
          <Group label="Ontology" items={NAV} active={active} onNavigate={() => setSheet(false)} badges={badges} />
          <Group label="Organization" items={ORG_NAV} active={active} onNavigate={() => setSheet(false)} />
          <div className="mt-3 border-t border-hairline pt-2">
            <UserMenu />
          </div>
        </div>
      </Dialog>

      <CommandPalette open={palette} onClose={() => setPalette(false)} views={PALETTE_VIEWS} />
    </div>
  );
}
