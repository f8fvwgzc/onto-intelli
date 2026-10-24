import { useState, type ReactNode } from "react";
import { ChevronRight, CircleAlert, Lock, TriangleAlert } from "lucide-react";

import { plural } from "../lib/format";
import type { Consequences, DiffEntry, Json, LikelyRename, LockViolation, PlanShapeDelta, Risk, SchemaPlan, ShapeResult } from "../lib/types";
import { DerivedFact, FactChip } from "./Facts";
import { Badge, cx } from "./ui";

export function PlanVerdict({ plan }: { plan: SchemaPlan }) {
  if (!plan.valid) return <Badge tone="danger">Invalid</Badge>;
  if ((plan.locked_violations ?? []).some((v) => !v.overridden)) return <Badge tone="danger">Locked</Badge>;
  if ((plan.needs_migration ?? []).length) return <Badge tone="danger">Needs migration</Badge>;
  if (plan.breaking) return <Badge tone="warning">Breaking, migrated</Badge>;
  // "Safe" only when nothing semantic argues otherwise; a risky or
  // non-conservative plan breaks no data but is not safe to wave through.
  const c = plan.consequences;
  const semanticDoubt = plan.risk === "medium" || plan.risk === "high" || (c ? (c.verdict ?? (c.conservative ? "conservative" : "not_conservative")) !== "conservative" : false);
  if (semanticDoubt) return <Badge tone="neutral">No data breaks</Badge>;
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
        {plan.risk ? <RiskWord risk={plan.risk} /> : null}
      </div>

      {(plan.risk_reasons ?? []).length ? (
        <ul className="-mt-2 flex flex-col gap-0.5 text-[13px] text-ink-2">
          {plan.risk_reasons!.map((r) => (
            <li key={r}>{r}</li>
          ))}
        </ul>
      ) : null}
      {(plan.locked_violations ?? []).length ? <LockList items={plan.locked_violations!} /> : null}

      {(plan.errors ?? []).length ? (
        <Notes tone="danger" title="Fix before applying" items={plan.errors!} />
      ) : null}
      {(plan.needs_migration ?? []).length ? (
        <Notes tone="danger" title="Breaks data in use: each needs a migration" items={plan.needs_migration!} />
      ) : null}
      {(plan.warnings ?? []).length ? <Notes tone="warning" title="Worth knowing" items={plan.warnings!} /> : null}
      {(plan.likely_renames ?? []).length ? <Renames items={plan.likely_renames!} /> : null}
      {plan.consequences ? <ConsequencesView c={plan.consequences} /> : null}
      {plan.shapes ? <ShapeDeltaView d={plan.shapes} /> : null}

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

const RISK_TONE: Record<Risk, "neutral" | "warning" | "danger"> = { low: "neutral", medium: "warning", high: "danger" };

/** The plan's risk word (an opinion of the planner, explained by its reasons). */
export function RiskWord({ risk }: { risk: Risk }) {
  return <Badge tone={RISK_TONE[risk]}>{risk} risk</Badge>;
}

function LockList({ items }: { items: LockViolation[] }) {
  const blocking = items.some((v) => !v.overridden);
  return (
    <div className={cx("rounded-card p-3", blocking ? "bg-danger-tint" : "bg-warning-tint")} role={blocking ? "alert" : undefined}>
      <p className={cx("flex items-center gap-1.5 text-[13px] font-medium", blocking ? "text-danger" : "text-ink")}>
        <Lock className="size-4" aria-hidden /> {blocking ? "Touches locked definitions" : "Locks overridden by an admin"}
      </p>
      <ul className="mt-1.5 flex flex-col gap-1.5 text-[13px] text-ink-2">
        {items.map((v) => (
          <li key={`${v.section}/${v.name}/${v.change}`}>
            <span className="font-mono text-ink">
              {v.section}/{v.name}
            </span>{" "}
            {v.reason}
            {v.overridden ? <span className="text-ink-3"> (overridden)</span> : null}
            <span className="block text-xs text-ink-3">
              locked by {v.locked_by}
              {v.lock_reason ? `: ${v.lock_reason}` : ""}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function Renames({ items }: { items: LikelyRename[] }) {
  return (
    <div>
      <p className="mb-1.5 text-[13px] font-semibold">Looks renamed</p>
      <ul className="flex flex-col gap-1.5 text-[13px]">
        {items.map((r) => (
          <li key={`${r.section}/${r.from}`} className="rounded-input bg-sunken px-2.5 py-1.5">
            <p className="flex flex-wrap items-baseline gap-x-1.5">
              <span className="font-mono text-ink">{r.from}</span>
              <span className="text-ink-3">to</span>
              <span className="font-mono text-ink">{r.to}</span>
              <span className="tnum ml-auto text-xs text-ink-3">score {r.score.toFixed(2)}</span>
            </p>
            {r.evidence?.length ? <p className="text-xs text-ink-3">{r.evidence.join(" · ")}</p> : null}
          </li>
        ))}
      </ul>
    </div>
  );
}

const CONSERVATIVITY = {
  conservative: { label: "Conservative", tone: "accent" },
  not_conservative: { label: "Not conservative", tone: "warning" },
  undetermined: { label: "Conservativity undetermined", tone: "neutral" },
} as const;

/** Derived facts that appear and disappear: the semantic side of the plan. */
function ConsequencesView({ c }: { c: Consequences }) {
  const verdict = c.verdict ?? (c.skipped || !c.fixpoint_reached ? "undetermined" : c.conservative ? "conservative" : "not_conservative");
  const v = CONSERVATIVITY[verdict];
  const byRule = Object.entries(c.by_rule ?? {}).sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  return (
    <section className="rounded-card bg-raised p-3 shadow-card">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <p className="flex-1 text-[13px] font-semibold">Consequences</p>
        {c.skipped ? null : <Badge tone={v.tone}>{v.label}</Badge>}
      </div>
      {c.skipped ? (
        <p className="mt-1 text-[13px] text-ink-2">Not computed: {c.skipped}</p>
      ) : (
        <>
          <p className="tnum mt-1 text-[13px] text-ink-2">
            {plural(c.added, "fact")} {c.added === 1 ? "appears" : "appear"}, {plural(c.removed, "fact")} {c.removed === 1 ? "disappears" : "disappear"}
            {c.new_over_existing_vocabulary ? `; ${plural(c.new_over_existing_vocabulary, "new fact")} in words the schema already had` : ""}.
            {c.fixpoint_reached ? "" : " A run stopped before its fixpoint: these counts are lower bounds."}
          </p>
          {byRule.length ? (
            <p className="mt-1.5 flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-ink-3">
              {byRule.map(([rule, n]) => (
                <span key={rule} className="tnum">
                  <span className="font-mono text-ink-2">{rule}</span> +{n}
                </span>
              ))}
            </p>
          ) : null}
          {(c.witnesses ?? []).length ? (
            <FactGroup title="New in existing vocabulary (why it is not conservative)">
              {c.witnesses!.map((f, i) => (
                <DerivedFact key={i} item={f} />
              ))}
            </FactGroup>
          ) : (c.samples_added ?? []).length ? (
            <FactGroup title="Facts that appear">
              {c.samples_added!.map((f, i) => (
                <DerivedFact key={i} item={f} />
              ))}
            </FactGroup>
          ) : null}
          {(c.samples_removed ?? []).length ? (
            <FactGroup title="Facts that disappear">
              {c.samples_removed!.map((f, i) => (
                <li key={i}>
                  <FactChip fact={f} className="line-through decoration-[var(--danger)]" />
                </li>
              ))}
            </FactGroup>
          ) : null}
        </>
      )}
      <p className="mt-2 text-xs text-ink-3">{c.means}</p>
    </section>
  );
}

function FactGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="mt-3">
      <p className="mb-1.5 text-xs font-medium text-ink-2">{title}</p>
      <ul className="flex flex-col gap-1.5">{children}</ul>
    </div>
  );
}

function listOf(v: ShapeResult[] | number | undefined): ShapeResult[] {
  return Array.isArray(v) ? v : [];
}

function countOf(v: ShapeResult[] | number | undefined): number {
  return Array.isArray(v) ? v.length : (v ?? 0);
}

/** Shape results the change creates and resolves on today's graph. */
function ShapeDeltaView({ d }: { d: PlanShapeDelta }) {
  const added = countOf(d.new_results);
  const resolved = countOf(d.resolved);
  const rows = [...listOf(d.new_results).map((r) => ({ r, kind: "new" as const })), ...listOf(d.resolved).map((r) => ({ r, kind: "resolved" as const }))];
  const samples = rows.length ? rows : (d.samples ?? []).map((r) => ({ r, kind: "new" as const }));
  return (
    <section className="rounded-card bg-raised p-3 shadow-card">
      <p className="text-[13px] font-semibold">Shapes</p>
      {d.skipped ? (
        <p className="mt-1 text-[13px] text-ink-2">Not computed: {d.skipped}</p>
      ) : (
        <p className="tnum mt-1 text-[13px] text-ink-2">
          {plural(added, "new result")}, {plural(resolved, "resolved result")}
          {d.before && d.after ? (
            <>
              {" "}
              · {d.before} → {d.after}
            </>
          ) : null}
          {d.complete === false ? " · partial (a report was cut at its limit)" : ""}
        </p>
      )}
      {samples.length ? (
        <ul className="mt-2 flex flex-col gap-1 text-[13px]">
          {samples.slice(0, 20).map(({ r, kind }, i) => (
            <li key={i} className="flex min-w-0 gap-2">
              <span className={cx("w-16 shrink-0 text-xs", kind === "new" ? "text-danger" : "text-success")}>{kind}</span>
              <span className="min-w-0">
                <span className="font-mono text-ink">{r.focus}</span> <span className="text-ink-2">{r.message}</span>
                <span className="block font-mono text-xs text-ink-3">
                  {r.shape} · {r.component}
                  {r.path ? ` · ${r.path}` : ""}
                </span>
              </span>
            </li>
          ))}
        </ul>
      ) : null}
      {d.means ? <p className="mt-2 text-xs text-ink-3">{d.means}</p> : null}
    </section>
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
