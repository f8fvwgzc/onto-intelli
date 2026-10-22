// The organization and project switcher of the console shell. Choosing a
// project makes its ref the session's tenant, so every page (and every query
// key, which carries the tenant) works on that project.

import { useEffect, useRef, useState } from "react";
import { Check, ChevronsUpDown, LayoutGrid, Plus } from "lucide-react";

import { useConsoleScope, type Project } from "../lib/console";
import { href, navigate, useRoute } from "../lib/router";
import { setSession } from "../lib/session";
import { cx } from "./ui";

export function selectProject(p: Project) {
  const ws = typeof p.settings?.default_workspace === "string" && p.settings.default_workspace ? p.settings.default_workspace : undefined;
  setSession({ tenant: p.ref, org: p.org_id, workspace: ws });
}

export function ProjectSwitcher({ compact, rail }: { compact?: boolean; rail?: boolean }) {
  const { me, project, org } = useConsoleScope();
  const route = useRoute();
  const [open, setOpen] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const items = () => Array.from(menu.current?.querySelectorAll<HTMLElement>("[data-item]") ?? []);
    requestAnimationFrame(() => (items().find((el) => el.getAttribute("aria-checked") === "true") ?? items()[0])?.focus());
    const onDown = (e: PointerEvent) => {
      if (!menu.current?.contains(e.target as Node) && !button.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        setOpen(false);
        button.current?.focus();
      } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const list = items();
        const i = list.indexOf(document.activeElement as HTMLElement);
        list[(i + (e.key === "ArrowDown" ? 1 : list.length - 1) + list.length) % list.length]?.focus();
      }
    };
    document.addEventListener("pointerdown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  if (!me) return null;
  const close = () => setOpen(false);

  return (
    <div className="relative min-w-0">
      <button
        ref={button}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Switch project. Current: ${project ? `${project.name} in ${org?.name}` : "none"}`}
        title={rail ? `${org?.name ?? ""} / ${project?.name ?? "No project"}` : undefined}
        onClick={() => setOpen((o) => !o)}
        className={cx(
          "flex w-full min-w-0 items-center gap-2 rounded-input text-left transition-colors duration-150 hover:bg-raised",
          compact ? "h-9 px-2" : rail ? "size-10 justify-center" : "h-11 px-2.5",
        )}
      >
        <span aria-hidden className="grid size-6 shrink-0 place-items-center rounded-[5px] bg-accent-tint text-[11px] font-semibold text-accent uppercase">
          {(project?.name ?? org?.name ?? "?").slice(0, 1)}
        </span>
        {rail ? null : (
          <span className="min-w-0 flex-1">
            {compact ? null : <span className="block truncate text-[11.5px] leading-4 text-ink-3">{org?.name ?? "No organization"}</span>}
            <span className="block truncate text-[13px] leading-5 font-medium text-ink">{project?.name ?? "Choose a project"}</span>
          </span>
        )}
        {rail ? null : <ChevronsUpDown className="size-4 shrink-0 text-ink-3" aria-hidden />}
      </button>
      {open ? (
        <div
          ref={menu}
          role="menu"
          aria-label="Projects"
          className={cx(
            "absolute z-40 mt-1 max-h-[min(70dvh,480px)] w-[min(288px,calc(100vw-32px))] overflow-y-auto overscroll-contain rounded-card bg-raised p-1 shadow-overlay",
            rail ? "left-full top-0 ml-2" : "left-0",
          )}
        >
          {me.orgs.map((o) => {
            const projects = me.projects.filter((p) => p.org_id === o.id);
            return (
              <div key={o.id} role="group" aria-label={o.name} className="py-1">
                <p className="flex items-center gap-2 px-2 pt-1 pb-1.5 text-xs text-ink-3">
                  <span className="min-w-0 flex-1 truncate font-medium">{o.name}</span>
                  <span>{o.role}</span>
                </p>
                {projects.map((p) => (
                  <button
                    key={p.id}
                    data-item
                    role="menuitemradio"
                    aria-checked={p.ref === project?.ref}
                    onClick={() => {
                      selectProject(p);
                      close();
                      if (route.path[0] === "projects") navigate(["overview"]);
                    }}
                    className="flex h-8 w-full items-center gap-2 rounded-input px-2 text-left text-[13px] text-ink hover:bg-sunken focus-visible:bg-sunken"
                  >
                    <span className="min-w-0 flex-1 truncate">{p.name}</span>
                    <span className="font-mono text-[11px] text-ink-3">{p.ref.slice(0, 6)}</span>
                    <Check className={cx("size-3.5 shrink-0 text-accent", p.ref !== project?.ref && "invisible")} aria-hidden />
                  </button>
                ))}
                {!projects.length ? <p className="px-2 pb-1 text-[13px] text-ink-3">No projects yet</p> : null}
              </div>
            );
          })}
          <div className="mt-1 border-t border-hairline pt-1">
            <a
              data-item
              role="menuitem"
              href={href(["projects"])}
              onClick={close}
              className="flex h-8 items-center gap-2 rounded-input px-2 text-[13px] text-ink-2 hover:bg-sunken hover:text-ink focus-visible:bg-sunken"
            >
              <LayoutGrid className="size-4" aria-hidden /> All projects
            </a>
            <a
              data-item
              role="menuitem"
              href={href(["projects"], { new: "1" })}
              onClick={close}
              className="flex h-8 items-center gap-2 rounded-input px-2 text-[13px] text-ink-2 hover:bg-sunken hover:text-ink focus-visible:bg-sunken"
            >
              <Plus className="size-4" aria-hidden /> New project
            </a>
          </div>
        </div>
      ) : null}
    </div>
  );
}
