import { useState } from "react";
import { ChevronRight, CircleAlert, TriangleAlert } from "lucide-react";

import { plural } from "../lib/format";
import type { DiffEntry, Json, SchemaPlan } from "../lib/types";
import { Badge, cx } from "./ui";

export function PlanVerdict({ plan }: { plan: SchemaPlan }) {
  if (!plan.valid) return <Badge tone="danger">Invalid</Badge>;
  if ((plan.needs_migration ?? []).length) return <Badge tone="danger">Needs migration</Badge>;
  if (plan.breaking) return <Badge tone="warning">Breaking, migrated</Badge>;
  return <Badge tone="accent">Safe</Badge>;
}

/** Impact report: what the change does to the live graph, and what it still needs. */
export function PlanView({ plan }: { plan: SchemaPlan }) {
  const m = plan.migration;
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[13px]">
        <PlanVerdict plan={plan} />
        <span className="tnum text-ink-2">{plural(plan.entities_affected, "entity", "entities")} in scope</span>
        <span className="tnum text-ink-2">{plural(plan.links_affected, "link")} in scope</span>
        <span className="text-ink-3">on schema v{plan.base_version}</span>
      </div>

      {(plan.errors ?? []).length ? (
        <Notes tone="danger" title="Fix before applying" items={plan.errors!} />
      ) : null}
      {(plan.needs_migration ?? []).length ? (
        <Notes tone="danger" title="Breaks data in use: each needs a migration" items={plan.needs_migration!} />
      ) : null}
      {(plan.warnings ?? []).length ? <Notes tone="warning" title="Worth knowing" items={plan.warnings!} /> : null}

      <ul className="flex flex-col divide-y divide-hairline rounded-card bg-raised shadow-card">
        {plan.changes.map((c) => (
          <li key={c.index} className="px-3 py-2.5">
            <div className="flex items-start gap-2">
              <span className={cx("mt-1.5 size-1.5 shrink-0 rounded-full", c.breaking ? "bg-danger" : "bg-[var(--gray-3)]")} aria-hidden />
              <div className="min-w-0 flex-1">
                <p className="text-[13px] text-ink">{c.summary}</p>
                <p className="tnum text-xs text-ink-3">
                  {plural(c.entities, "entity", "entities")} · {plural(c.links, "link")}
                  {c.sample?.length ? ` · e.g. ${c.sample.slice(0, 3).join(", ")}` : ""}
                </p>
                {c.reasons?.map((r) => (
                  <p key={r} className="mt-1 text-xs text-ink-2">
                    {r}
                  </p>
                ))}
              </div>
            </div>
          </li>
        ))}
      </ul>

      {m.mutations > 0 ? (
        <p className="tnum text-[13px] text-ink-2">
          Migration: {m.entities_updated} updated, {m.entities_retyped} retyped, {m.entities_deleted} deleted entities; {m.links_deleted} links removed,{" "}
          {m.links_moved} moved. Committed with the schema version, in one transaction.
        </p>
      ) : null}

      {plan.diff.entries.length ? <DiffList entries={plan.diff.entries} /> : null}
    </div>
  );
}

function Notes({ tone, title, items }: { tone: "danger" | "warning"; title: string; items: string[] }) {
  const Icon = tone === "danger" ? CircleAlert : TriangleAlert;
  return (
    <div className={cx("rounded-card p-3", tone === "danger" ? "bg-danger-tint" : "bg-warning-tint")} role={tone === "danger" ? "alert" : undefined}>
      <p className={cx("flex items-center gap-1.5 text-[13px] font-medium", tone === "danger" ? "text-danger" : "text-ink")}>
        <Icon className="size-4" aria-hidden /> {title}
      </p>
      <ul className="mt-1.5 flex list-disc flex-col gap-1 pl-5 text-[13px] text-ink-2">
        {items.map((i) => (
          <li key={i}>{i}</li>
        ))}
      </ul>
    </div>
  );
}

export function DiffList({ entries }: { entries: DiffEntry[] }) {
  return (
    <div>
      <p className="mb-1.5 text-[13px] font-semibold">Diff</p>
      <ul className="flex flex-col gap-1">
        {entries.map((e) => (
          <DiffRow key={`${e.section}.${e.name}`} entry={e} />
        ))}
      </ul>
    </div>
  );
}

function DiffRow({ entry }: { entry: DiffEntry }) {
  const [open, setOpen] = useState(false);
  const sign = entry.change === "added" ? "+" : entry.change === "removed" ? "−" : "~";
  return (
    <li className="rounded-input bg-sunken">
      <button onClick={() => setOpen((o) => !o)} aria-expanded={open} className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[13px]">
        <ChevronRight className={cx("size-3.5 shrink-0 text-ink-3 transition-transform duration-150", open && "rotate-90")} aria-hidden />
        <span className={cx("w-3 font-mono", entry.change === "removed" ? "text-danger" : entry.change === "added" ? "text-success" : "text-ink-2")}>{sign}</span>
        <span className="min-w-0 truncate font-mono">
          <span className="text-ink-2">{entry.section}.</span>
          <span className="text-ink">{entry.name}</span>
        </span>
        {entry.fields?.length ? <span className="ml-auto truncate font-mono text-xs text-ink-3">{entry.fields.join(", ")}</span> : null}
      </button>
      {open ? (
        <div className="grid gap-2 px-2.5 pb-2.5 md:grid-cols-2">
          <JsonBlock label="Before" value={entry.before} />
          <JsonBlock label="After" value={entry.after} />
        </div>
      ) : null}
    </li>
  );
}

function JsonBlock({ label, value }: { label: string; value?: Json }) {
  return (
    <div className="min-w-0">
      <p className="mb-1 text-xs text-ink-3">{label}</p>
      <pre className="max-h-64 overflow-auto rounded-input bg-raised p-2 font-mono text-[11.5px] leading-relaxed text-ink shadow-card">
        {value === undefined ? "—" : JSON.stringify(value, null, 2)}
      </pre>
    </div>
  );
}
