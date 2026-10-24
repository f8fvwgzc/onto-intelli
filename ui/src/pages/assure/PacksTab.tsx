import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowRight, Check, FileSearch, PackagePlus, Send } from "lucide-react";

import { PlanView } from "../../components/PlanView";
import { Badge, Button, Checkbox, Dialog, EmptyState, ErrorState, Panel, Section, Skeleton, Spinner, cx } from "../../components/ui";
import { call, errorText } from "../../lib/api";
import { plural } from "../../lib/format";
import { useScope, useWhoami } from "../../lib/queries";
import { href, navigate } from "../../lib/router";
import type { InstallMode, PackCounts, PackDetail, PackInfo, PackInstalled, SchemaPlan } from "../../lib/types";
import { Means } from "./common";

/** `supply_chain` → `Supply chain`. */
function humanize(s: string): string {
  const t = s.replace(/[_-]+/g, " ");
  return t.charAt(0).toUpperCase() + t.slice(1);
}

function countLine(c: PackCounts): string {
  return [
    plural(c.object_types, "type"),
    plural(c.link_types, "link"),
    plural(c.rules, "rule"),
    plural(c.shapes, "shape"),
  ].join(" · ");
}

export function PacksTab({ id }: { id?: string }) {
  const scope = useScope();
  const list = useQuery<{ items: PackInfo[] }>({ queryKey: ["packs", ...scope], queryFn: () => call("pack.list") });
  const close = () => navigate(["assure", "packs"]);

  return (
    <div className="flex min-h-0 flex-1 flex-col md:flex-row">
      <div className="min-h-0 flex-1 overflow-y-auto px-4 pt-4 pb-10 md:px-5">
        <p className="max-w-[75ch] text-[13px] text-ink-2">
          A pack layers one industry's types, relations, rules and shapes on the schema. Its rules are assumptions: what they derive is labelled with the rule, never
          stored, and a certificate citing them is only entailed under those rules. Standards it lists are vocabulary it borrows, not compliance it claims.
        </p>
        {list.isLoading ? (
          <div className="mt-4 grid grid-cols-[repeat(auto-fill,minmax(250px,1fr))] gap-3">
            {[0, 1, 2].map((i) => (
              <Skeleton key={i} className="h-36" />
            ))}
          </div>
        ) : list.error ? (
          <div className="mt-4">
            <ErrorState error={list.error} retry={() => void list.refetch()} />
          </div>
        ) : !list.data?.items.length ? (
          <EmptyState title="No packs registered">This build carries no industry packs.</EmptyState>
        ) : (
          <ul className="mt-4 grid grid-cols-[repeat(auto-fill,minmax(250px,1fr))] gap-3">
            {list.data.items.map((p) => (
              <li key={p.id}>
                <PackCard pack={p} selected={p.id === id} />
              </li>
            ))}
          </ul>
        )}
      </div>
      <Panel open={Boolean(id)} onClose={close} wide title={<PanelTitle id={id} items={list.data?.items} />}>
        {id ? <PackDrawer key={id} id={id} info={list.data?.items.find((p) => p.id === id)} /> : null}
      </Panel>
    </div>
  );
}

function PanelTitle({ id, items }: { id?: string; items?: PackInfo[] }) {
  const p = items?.find((x) => x.id === id);
  return (
    <div className="min-w-0">
      <p className="truncate text-[15px] font-semibold tracking-tight">{p?.title || id}</p>
      <p className="truncate font-mono text-xs text-ink-3">
        builtin:{id}
        {p?.version ? ` · ${p.version}` : ""}
      </p>
    </div>
  );
}

function PackCard({ pack, selected }: { pack: PackInfo; selected: boolean }) {
  return (
    <a
      href={href(["assure", "packs", pack.id])}
      aria-current={selected ? "true" : undefined}
      className={cx(
        "flex h-full flex-col gap-2 rounded-card bg-raised p-4 shadow-card transition-[box-shadow,background-color] duration-150 hover:bg-sunken",
        selected && "outline-2 outline-accent",
      )}
    >
      <div className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          {pack.industry ? <p className="truncate text-xs text-ink-3">{humanize(pack.industry)}</p> : null}
          <p className="truncate text-[14px] font-semibold tracking-tight text-ink">{pack.title || pack.id}</p>
        </div>
        <span className="tnum shrink-0 font-mono text-xs text-ink-3">{pack.version}</span>
      </div>
      <p className="tnum text-[13px] text-ink-2">{countLine(pack.counts)}</p>
      {pack.standards?.length ? (
        <p className="line-clamp-2 text-xs text-ink-3" title={pack.standards.join(", ")}>
          Borrows from {pack.standards.join(", ")}
        </p>
      ) : null}
      <div className="mt-auto flex flex-wrap items-center gap-1.5 pt-1">
        {pack.installed ? (
          <Badge tone="accent">
            <Check className="mr-1 size-3" aria-hidden /> Installed
          </Badge>
        ) : pack.blocked ? (
          <Badge tone="danger">Blocked</Badge>
        ) : pack.missing_requires?.length ? (
          <Badge tone="warning">Needs {pack.missing_requires.join(", ")}</Badge>
        ) : (
          <span className="tnum text-xs text-ink-3">{plural(pack.pending_changes, "schema change")} to install</span>
        )}
      </div>
    </a>
  );
}

function PackDrawer({ id, info }: { id: string; info?: PackInfo }) {
  const scope = useScope();
  const qc = useQueryClient();
  const me = useWhoami().data;
  const [planOpen, setPlanOpen] = useState(false);
  const [sample, setSample] = useState(false);
  const detail = useQuery<PackDetail>({ queryKey: ["pack", id, ...scope], queryFn: () => call<PackDetail>("pack.show", { id }) });
  const plan = useQuery<SchemaPlan>({ queryKey: ["pack-plan", id, ...scope], queryFn: () => call<SchemaPlan>("pack.plan", { id }), enabled: planOpen });
  const install = useMutation({
    mutationFn: (mode: InstallMode) => call<PackInstalled>("pack.install", { id, mode, sample: mode === "apply" && sample }),
    onSuccess: () => {
      for (const k of ["packs", "pack", "pack-plan", "schema", "overview", "proposals", "reason", "shapes", "lint"]) void qc.invalidateQueries({ queryKey: [k] });
    },
  });

  if (detail.isLoading) return <div className="py-6"><Spinner /></div>;
  if (detail.error) return <div className="py-4"><ErrorState error={detail.error} retry={() => void detail.refetch()} /></div>;
  const d = detail.data!;
  const canApply = Boolean(me?.capabilities.apply_schema);
  const canPropose = Boolean(me?.capabilities.propose);
  const blocked = info?.blocked ?? (info?.missing_requires?.length ? `needs ${info.missing_requires.join(", ")}` : undefined);
  const done = install.data;
  const lint = d.lint?.counts;

  return (
    <div className="pt-3">
      {d.description ? <p className="text-[13px] leading-relaxed text-ink">{d.description}</p> : null}
      <p className="tnum mt-2 text-xs text-ink-3">
        {countLine(d.counts)} · {plural(d.counts.sample_entities, "sample entity", "sample entities")}
        {d.requires?.length ? ` · layers on ${d.requires.join(", ")}` : ""}
      </p>

      <div className="mt-4 rounded-card bg-sunken p-3">
        {info?.installed ? (
          <p className="flex items-center gap-1.5 text-[13px] text-ink">
            <Check className="size-4 text-accent" aria-hidden /> Every definition of this pack is in the schema.
          </p>
        ) : blocked ? (
          <p className="text-[13px] text-danger">Cannot be layered on this schema: {blocked}</p>
        ) : (
          <p className="tnum text-[13px] text-ink-2">Installing makes {plural(info?.pending_changes ?? 0, "schema change")}. Plan it first to see what it derives.</p>
        )}
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <Button size="sm" icon={<FileSearch className="size-4" aria-hidden />} onClick={() => setPlanOpen(true)}>
            Plan
          </Button>
          {canApply ? (
            <Button size="sm" variant="primary" icon={<PackagePlus className="size-4" aria-hidden />} busy={install.isPending && install.variables === "apply"} disabled={install.isPending || Boolean(blocked)} onClick={() => install.mutate("apply")}>
              Install
            </Button>
          ) : null}
          {canPropose ? (
            <Button
              size="sm"
              variant={canApply ? "ghost" : "primary"}
              icon={<Send className="size-4" aria-hidden />}
              busy={install.isPending && install.variables === "propose"}
              disabled={install.isPending || Boolean(blocked) || info?.installed}
              onClick={() => install.mutate("propose")}
            >
              {canApply ? "Propose instead" : "Propose"}
            </Button>
          ) : null}
          {!canApply && !canPropose ? <p className="text-[13px] text-ink-3">Your role can read packs but not install or propose them.</p> : null}
        </div>
        {canApply ? (
          <div className="mt-3">
            <Checkbox label="Load sample data into this workspace" checked={sample} onChange={setSample} />
            <p className="mt-1 pl-6 text-xs text-ink-3">Fills gaps only: existing entities keep their values. A proposal cannot load samples until it is applied.</p>
          </div>
        ) : null}
        {install.error ? <p className="mt-2 text-[13px] text-danger">{errorText(install.error)}</p> : null}
        {done ? <InstallResult r={done} /> : null}
      </div>

      {d.sample_note ? (
        <Section title="Sample data">
          <p className="text-[13px] leading-relaxed text-ink-2">{d.sample_note}</p>
        </Section>
      ) : null}

      {lint ? (
        <Section title="Lint, layered on this schema">
          <p className="tnum text-[13px] text-ink-2">
            {plural(lint.error, "error")}, {plural(lint.warning, "warning")}, {plural(lint.info, "note")}
          </p>
        </Section>
      ) : null}

      <ItemList title="Object types" items={d.object_types} />
      {d.link_types.length ? (
        <Section title={`Link types · ${d.link_types.length}`}>
          <ul className="flex flex-col gap-1.5">
            {d.link_types.map((l) => (
              <li key={l.name} className="text-[13px]">
                <p className="flex flex-wrap items-center gap-x-1.5 font-mono text-[12.5px]">
                  <span className="text-ink-2">{l.from.join(" | ") || "any"}</span>
                  <ArrowRight className="size-3 text-ink-3" aria-hidden />
                  <span className="font-medium text-ink">{l.name}</span>
                  <ArrowRight className="size-3 text-ink-3" aria-hidden />
                  <span className="text-ink-2">{l.to.join(" | ") || "any"}</span>
                  {l.transitive ? <Badge>transitive</Badge> : null}
                  {l.dependency ? <Badge>dependency</Badge> : null}
                </p>
                {l.description ? <p className="text-xs text-ink-3">{l.description}</p> : null}
              </li>
            ))}
          </ul>
        </Section>
      ) : null}
      <ItemList title="Interfaces" items={d.interfaces} />
      {d.rules.length ? (
        <Section title={`Rules · ${d.rules.length}`} aside={<span className="text-xs text-ink-3">supplied: assumptions, not facts</span>}>
          <ul className="flex flex-col gap-2">
            {d.rules.map((r) => (
              <li key={r.name}>
                <p className="flex items-center gap-1.5 text-[13px] font-medium">
                  <span className="font-mono">{r.name}</span>
                  <Badge>{r.form}</Badge>
                </p>
                <pre className="mt-1 overflow-x-auto rounded-input bg-sunken px-2 py-1.5 font-mono text-[11.5px] leading-relaxed text-ink">{r.text}</pre>
                {r.description ? <p className="mt-0.5 text-xs text-ink-3">{r.description}</p> : null}
              </li>
            ))}
          </ul>
          {d.derived_classes.length || d.derived_relations.length ? (
            <p className="mt-2 text-xs text-ink-3">
              Derives {[...d.derived_classes.map((c) => `#${c}`), ...d.derived_relations].join(", ")}
            </p>
          ) : null}
        </Section>
      ) : null}
      <ItemList title="Shapes" items={d.shapes} />
      <ItemList title="Metrics" items={d.metrics} />
      <ItemList title="Action types" items={d.action_types} />
      <Means>{d.means}</Means>

      <Dialog open={planOpen} onClose={() => setPlanOpen(false)} title={`Plan: install ${d.title || id}`} wide>
        {plan.isLoading ? <Spinner label="Planning" /> : plan.error ? <ErrorState error={plan.error} retry={() => void plan.refetch()} /> : plan.data ? <PlanView plan={plan.data} /> : null}
      </Dialog>
    </div>
  );
}

function ItemList({ title, items }: { title: string; items: { name: string; label?: string; description?: string }[] }) {
  if (!items.length) return null;
  return (
    <Section title={`${title} · ${items.length}`}>
      <ul className="flex flex-col gap-1">
        {items.map((i) => (
          <li key={i.name} className="text-[13px]">
            <span className="font-mono text-[12.5px] text-ink">{i.name}</span>
            {i.label && i.label !== i.name ? <span className="text-ink-2"> · {i.label}</span> : null}
            {i.description ? <span className="block text-xs text-ink-3">{i.description}</span> : null}
          </li>
        ))}
      </ul>
    </Section>
  );
}

function InstallResult({ r }: { r: PackInstalled }) {
  if (r.already_installed && !r.sample_entities) return <p className="mt-2 text-[13px] text-ink-2">Already installed: nothing changed.</p>;
  return (
    <div className="mt-2 text-[13px] text-ink-2" role="status">
      {r.mode === "propose" && r.proposal ? (
        <p>
          Filed as a proposal with {plural(r.changes, "change")}.{" "}
          <a className="text-accent underline-offset-2 hover:underline" href={href(["proposals", r.proposal.id])}>
            Review it
          </a>
        </p>
      ) : r.changes ? (
        <p className="tnum">
          Applied {plural(r.changes, "change")}
          {r.version ? ` as schema version ${r.version.version}` : ""}.
        </p>
      ) : null}
      {r.sample_entities ? (
        <p className="tnum">
          Loaded {plural(r.sample_entities, "sample entity", "sample entities")} and {plural(r.sample_links, "link")}
          {r.sample_links_skipped ? `; kept ${plural(r.sample_links_skipped, "curated link")}` : ""}.
        </p>
      ) : null}
    </div>
  );
}
