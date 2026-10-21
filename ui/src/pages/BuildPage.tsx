import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient, type UseMutationResult, type UseQueryResult } from "@tanstack/react-query";
import { ArrowRight, Bot, ChevronRight, CircleAlert, CircleCheck, Database, MessageSquareText, Plus, RefreshCw, Sparkles } from "lucide-react";

import { PlanView } from "../components/PlanView";
import { PageHeader } from "../components/Shell";
import { Badge, Button, Checkbox, Dialog, EmptyState, ErrorState, Field, Input, Section, Segmented, Select, Skeleton, Spinner, TagInput, Textarea, cx } from "../components/ui";
import { call, errorText } from "../lib/api";
import { ago, display, plural } from "../lib/format";
import { useSchema, useScope, useWhoami } from "../lib/queries";
import { href, navigate } from "../lib/router";
import type { ActionType, Advice, ConnectorInfo, CrawlReport, DraftAction, DraftLink, DraftObject, Entity, OntologyDraft, Refined, SchemaDoc, SchemaPlan, Suggestion } from "../lib/types";
import { TypeBadge } from "./ExplorePage";

const NAME = /^[a-z][a-z0-9_]*$/;
const EVERY = [
  { value: 0, label: "On demand only" },
  { value: 900, label: "Every 15 minutes" },
  { value: 3600, label: "Every hour" },
  { value: 86400, label: "Every day" },
];

interface Options {
  source: string;
  tables: string[];
  exclude: string[];
  noDefaultExclude: boolean;
  prefix: string;
  sync: boolean;
  actions: boolean;
  views: boolean;
}

/** What the reviewer changed: left-out items (by drafted name) and new type names. */
interface Edits {
  objects: Set<string>;
  props: Set<string>;
  links: Set<string>;
  actions: Set<string>;
  names: Record<string, string>;
}

const noEdits = (): Edits => ({ objects: new Set(), props: new Set(), links: new Set(), actions: new Set(), names: {} });

function draftInput(o: Options) {
  return {
    source: o.source,
    tables: o.tables.length ? o.tables : undefined,
    exclude: o.exclude.length ? o.exclude : undefined,
    no_default_exclude: o.noDefaultExclude && !o.exclude.length ? true : undefined,
    prefix: o.prefix.trim() || undefined,
    sync: o.sync,
    actions: o.actions,
    views: o.views,
  };
}

/**
 * The draft as reviewed: left-out object types take their links and actions
 * with them, and a renamed type is renamed wherever the draft names it. The
 * server rebuilds the change set from these parts.
 */
function applyEdits(d: OntologyDraft, e: Edits): OntologyDraft {
  const rn = (n: string) => e.names[n]?.trim() || n;
  const objects = d.objects
    .filter((o) => !e.objects.has(o.name))
    .map((o) => ({ ...o, name: rn(o.name), properties: o.properties.filter((p) => !e.props.has(`${o.name}.${p.name}`)) }));
  const kept = new Set(d.objects.filter((o) => !e.objects.has(o.name)).map((o) => o.name));
  const links = d.links.filter((l) => linkKept(l, e, kept)).map((l) => ({ ...l, from: rn(l.from), to: rn(l.to) }));
  const linkNames = new Set(links.map((l) => l.name));
  const actions = d.actions
    .filter((a) => actionKept(a, e, kept, linkNames))
    .map((a) => ({ ...a, object: rn(a.object), name: rn(a.object) !== a.object && a.name.endsWith(`_${a.object}`) ? `${a.name.slice(0, -a.object.length)}${rn(a.object)}` : a.name }));
  return { ...d, objects, links, actions };
}

function linkKept(l: DraftLink, e: Edits, kept: Set<string>) {
  return !e.links.has(l.name) && kept.has(l.from) && kept.has(l.to);
}

function actionKept(a: DraftAction, e: Edits, kept: Set<string>, links: Set<string>) {
  return !e.actions.has(a.name) && kept.has(a.object) && (a.kind !== "relink" || !a.link || links.has(a.link));
}

/**
 * Problems with the new type names, by drafted name: names the draft uses
 * twice, and names of object or action types already in the schema, which
 * the draft would replace. An object type backed by the same table (an
 * earlier draft of this source) is the one exception, and so are the
 * actions on it.
 */
function nameErrors(d: OntologyDraft, e: Edits, schema: SchemaDoc | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  const seen = new Map<string, string>();
  const ours = (n: string, table: string) => {
    const b = schema?.object_types[n]?.backing;
    return !!b && b.source === d.source && b.table === table;
  };
  const actsOn = (a: ActionType, kind: string) =>
    Object.values(a.parameters ?? {}).some((p) => p.kinds?.includes(kind)) || a.effects.some((x) => (x as { create?: { kind?: string } } | null)?.create?.kind === kind);
  for (const o of d.objects) {
    if (e.objects.has(o.name)) continue;
    const n = e.names[o.name]?.trim() || o.name;
    if (!NAME.test(n)) out[o.name] = "Lowercase letters, digits and _, starting with a letter";
    else if (seen.has(n)) out[o.name] = `Also the name of ${seen.get(n)}`;
    else if (schema?.interfaces[n]) out[o.name] = `${n} is an interface in the schema`;
    else if (schema?.object_types[n] && !ours(n, o.table)) {
      const b = schema.object_types[n].backing;
      out[o.name] = `An existing object type${b ? ` (from ${b.source} ${b.table})` : ""}: applying would replace it`;
    } else if (n !== o.name) {
      // Actions follow the rename (create_x → create_n); an existing action
      // of that name must already act on this type.
      const clash = d.actions
        .filter((a) => a.object === o.name && a.name.endsWith(`_${o.name}`))
        .map((a) => `${a.name.slice(0, -o.name.length)}${n}`)
        .find((a) => schema?.action_types[a] && !(ours(n, o.table) && actsOn(schema.action_types[a], n)));
      if (clash) out[o.name] = `The action ${clash} exists already: applying would replace it`;
    }
    seen.set(n, o.name);
  }
  return out;
}

function isAdmin(roles: string[] | undefined) {
  return !!roles?.some((r) => r === "admin" || r === "system");
}

export function BuildPage() {
  const me = useWhoami().data;
  const scope = useScope();
  const qc = useQueryClient();
  const admin = isAdmin(me?.principal.roles);
  const schema = useSchema().data;

  const connectors = useQuery<{ connectors: ConnectorInfo[] }>({ queryKey: ["connectors", ...scope], queryFn: () => call("connector.list") });
  const sources = useQuery<{ entities: Entity[] }>({
    queryKey: ["data-sources", ...scope],
    queryFn: () => call("entity.find", { kind: "data_source", limit: 200 }),
  });
  const sourceKeys = useMemo(() => {
    const keys = new Set<string>();
    for (const c of connectors.data?.connectors ?? []) keys.add(c.spec.name);
    for (const e of sources.data?.entities ?? []) keys.add(e.id.replace(/^data_source:/, ""));
    return [...keys].sort();
  }, [connectors.data, sources.data]);

  const [opts, setOpts] = useState<Options>({ source: "", tables: [], exclude: [], noDefaultExclude: false, prefix: "", sync: true, actions: true, views: false });
  const source = opts.source || sourceKeys[0] || "";
  const [draft, setDraft] = useState<OntologyDraft | null>(null);
  const [refined, setRefined] = useState<Refined | null>(null);
  const [edits, setEdits] = useState<Edits>(noEdits);
  const [adding, setAdding] = useState(false);
  const [reviewing, setReviewing] = useState(false);
  const [applied, setApplied] = useState<{ version: number; source: string; hint?: string; synced: boolean } | null>(null);
  const [instructions, setInstructions] = useState("");

  const start = (d: OntologyDraft, r: Refined | null) => {
    setDraft(d);
    setRefined(r);
    setEdits(noEdits());
    setApplied(null);
  };
  const drafting = useMutation({
    mutationFn: () => call<OntologyDraft>("ontology.draft", draftInput({ ...opts, source })),
    onSuccess: (d) => start(d, null),
  });
  const refining = useMutation({
    mutationFn: () => call<Refined>("ontology.refine", { ...draftInput({ ...opts, source }), instructions: instructions.trim() || undefined }),
    onSuccess: (r) => start(r.draft, r),
  });
  const crawl = useMutation({
    mutationFn: (name: string) => call<{ crawls: CrawlReport[] }>("connector.crawl", { name }),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: ["connectors"] });
      void qc.invalidateQueries({ queryKey: ["data-sources"] });
      void qc.invalidateQueries({ queryKey: ["object-types"] });
      void qc.invalidateQueries({ queryKey: ["explore"] });
      void qc.invalidateQueries({ queryKey: ["overview"] });
    },
  });

  const reviewed = useMemo(() => (draft ? applyEdits(draft, edits) : null), [draft, edits]);
  const errors = useMemo(() => (draft ? nameErrors(draft, edits, schema) : {}), [draft, edits, schema]);
  const hasConnector = (name: string) => !!connectors.data?.connectors.some((c) => c.spec.name === name);
  const busy = drafting.isPending || refining.isPending;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title="Builder" context="Connect a database, draft object, link and action types from its catalog, review them, then apply or propose">
        {admin ? (
          <Button size="sm" icon={<Plus className="size-4" />} onClick={() => setAdding(true)}>
            Add connector
          </Button>
        ) : null}
      </PageHeader>
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto grid w-full max-w-[1280px] gap-6 px-4 py-5 md:px-6 xl:grid-cols-[360px_minmax(0,1fr)]">
          <div className="grid min-w-0 content-start gap-6 md:grid-cols-2 xl:grid-cols-1">
            <Connectors q={connectors} admin={admin} crawl={crawl} />

            <section aria-labelledby="draft-heading" className="min-w-0 rounded-card bg-raised p-4 shadow-card">
              <h2 id="draft-heading" className="text-[13px] font-semibold">
                Draft an ontology
              </h2>
              <p className="mt-0.5 text-xs text-ink-3">From a crawled source: tables become object types, foreign keys and join tables become link types. No AI, no tokens.</p>
              <div className="mt-4 flex flex-col gap-3.5">
                <Field label="Source" hint={sourceKeys.length ? undefined : "Nothing crawled yet: add a connector and crawl it."}>
                  {(id) => (
                    <Select id={id} value={source} onChange={(e) => setOpts({ ...opts, source: e.target.value })} disabled={!sourceKeys.length}>
                      {sourceKeys.map((k) => (
                        <option key={k} value={k}>
                          {k}
                        </option>
                      ))}
                    </Select>
                  )}
                </Field>
                <Field label="Tables" hint="schema.table names or globs such as sales.*; empty takes every table">
                  {() => <TagInput value={opts.tables} onChange={(tables) => setOpts({ ...opts, tables })} placeholder="All tables" />}
                </Field>
                <Field label="Leave out" hint={opts.exclude.length || opts.noDefaultExclude ? undefined : "Default: staging.*, *_tmp, *_old, *_bak, *_backup"}>
                  {() => <TagInput value={opts.exclude} onChange={(exclude) => setOpts({ ...opts, exclude })} placeholder={opts.noDefaultExclude ? "Nothing" : "Default list"} />}
                </Field>
                {!opts.exclude.length ? <Checkbox label="Leave nothing out by default" checked={opts.noDefaultExclude} onChange={(noDefaultExclude) => setOpts({ ...opts, noDefaultExclude })} /> : null}
                <Field label="Name prefix" hint="Put before every object type name: crm → crm_customer">
                  {(id) => <Input id={id} value={opts.prefix} onChange={(e) => setOpts({ ...opts, prefix: e.target.value })} placeholder="None" className="font-mono" />}
                </Field>
                <div className="flex flex-col gap-2">
                  <Checkbox label="Sync rows as objects" checked={opts.sync} onChange={(sync) => setOpts({ ...opts, sync })} />
                  <Checkbox label="Draft action types" checked={opts.actions} onChange={(actions) => setOpts({ ...opts, actions })} />
                  <Checkbox label="Model views too" checked={opts.views} onChange={(views) => setOpts({ ...opts, views })} />
                </div>
                {drafting.error ? <p className="text-[13px] text-danger">{errorText(drafting.error)}</p> : null}
                <Button variant="primary" busy={drafting.isPending} disabled={!source || busy} onClick={() => drafting.mutate()}>
                  {draft ? "Draft again" : "Draft"}
                </Button>
              </div>
            </section>

            {source ? (
              <div className="md:col-span-2 xl:col-span-1">
                <Advisor source={source} />
              </div>
            ) : null}
          </div>

          <div className="min-w-0">
            {applied ? (
              <AppliedBanner
                applied={applied}
                connector={hasConnector(applied.source)}
                canCrawl={admin && hasConnector(applied.source)}
                crawl={crawl}
                onDismiss={() => setApplied(null)}
              />
            ) : null}
            {!draft || !reviewed ? (
              <div className="rounded-card bg-raised shadow-card">
                {busy ? (
                  <div className="flex flex-col gap-2 p-5">
                    <Skeleton className="h-5 w-1/3" />
                    <Skeleton className="h-24 w-full" />
                    <Skeleton className="h-24 w-full" />
                  </div>
                ) : (
                  <EmptyState title="No draft yet">Pick a source and draft it. You review every object type, property, link and action, with the reason it was proposed, before anything changes.</EmptyState>
                )}
              </div>
            ) : (
              <Review
                draft={draft}
                reviewed={reviewed}
                edits={edits}
                errors={errors}
                setEdits={setEdits}
                refined={refined}
                instructions={instructions}
                setInstructions={setInstructions}
                refining={refining}
                onPlan={() => setReviewing(true)}
              />
            )}
          </div>
        </div>
      </div>

      {adding ? <AddConnector onClose={() => setAdding(false)} onAdded={(name) => setOpts({ ...opts, source: name })} /> : null}
      {reviewing && reviewed ? (
        <PlanDialog
          draft={reviewed}
          canApply={!!me?.capabilities.apply_schema}
          canPropose={!!me?.capabilities.propose}
          onClose={() => setReviewing(false)}
          onApplied={(version) => {
            setReviewing(false);
            // A crawl run before the apply synced no rows of the new types.
            crawl.reset();
            setApplied({ version, source: reviewed.source, synced: reviewed.objects.some((o) => o.sync) });
            void qc.invalidateQueries({ queryKey: ["schema"] });
            void qc.invalidateQueries({ queryKey: ["object-types"] });
            void qc.invalidateQueries({ queryKey: ["whoami"] });
          }}
          onProposed={(id) => {
            void qc.invalidateQueries({ queryKey: ["proposals"] });
            navigate(["proposals", id]);
          }}
        />
      ) : null}
    </div>
  );
}

// ── connectors ───────────────────────────────────────────────────────────

type CrawlMutation = UseMutationResult<{ crawls: CrawlReport[] }, Error, string>;

function Connectors({ q, admin, crawl }: { q: UseQueryResult<{ connectors: ConnectorInfo[] }>; admin: boolean; crawl: CrawlMutation }) {
  const list = q.data?.connectors ?? [];
  return (
    <section aria-labelledby="connectors-heading" className="min-w-0 self-start rounded-card bg-raised shadow-card">
      <div className="flex items-center gap-2 border-b border-hairline px-4 py-3">
        <Database className="size-4 text-ink-3" aria-hidden />
        <h2 id="connectors-heading" className="flex-1 text-[13px] font-semibold">
          Connectors
        </h2>
        <span className="tnum text-xs text-ink-3">{list.length}</span>
      </div>
      {q.isLoading ? (
        <div className="p-4">
          <Skeleton className="h-12 w-full" />
        </div>
      ) : q.error ? (
        <div className="p-3">
          <ErrorState error={q.error} retry={() => q.refetch()} />
        </div>
      ) : list.length === 0 ? (
        <p className="px-4 py-4 text-[13px] text-ink-3">{admin ? "No database connected yet. Add one: metadata is crawled read-only, rows only for object types you approve." : "No database connected yet; an admin adds connectors."}</p>
      ) : (
        <ul className="divide-y divide-hairline">
          {list.map((c) => {
            const pending = crawl.isPending && crawl.variables === c.spec.name;
            const report = crawl.data?.crawls.find((r) => r.connector === c.spec.name) ?? c.last ?? undefined;
            const failed = crawl.error && crawl.variables === c.spec.name ? errorText(crawl.error) : c.last_error;
            return (
              <li key={c.spec.name} className="px-4 py-3">
                <div className="flex items-start gap-2">
                  <div className="min-w-0 flex-1">
                    <p className="flex items-center gap-2 text-[13px]">
                      <span className="font-mono font-medium text-ink">{c.spec.name}</span>
                      {c.spec.title ? <span className="truncate text-ink-2">{c.spec.title}</span> : null}
                      {failed ? <Badge tone="danger">error</Badge> : c.last_success_at ? <Badge tone="accent">ok</Badge> : <Badge>never crawled</Badge>}
                    </p>
                    <p className="mt-0.5 truncate font-mono text-xs text-ink-3" title={c.spec.url}>
                      {c.spec.kind} · {c.spec.url}
                    </p>
                    <p className="mt-0.5 text-xs text-ink-3">
                      {c.last_crawl_at ? `crawled ${ago(c.last_crawl_at)}` : "not crawled"} · {EVERY.find((e) => e.value === c.spec.every_secs)?.label.toLowerCase() ?? `every ${c.spec.every_secs}s`}
                      {c.spec.schemas.length ? ` · ${c.spec.schemas.join(", ")}` : ""} · {plural(c.assets, "asset")}
                      {c.spec.map_grants ? " · grants → ACLs" : ""}
                    </p>
                  </div>
                  {admin ? (
                    <Button size="sm" icon={<RefreshCw className="size-3.5" />} busy={pending} disabled={crawl.isPending} onClick={() => crawl.mutate(c.spec.name)}>
                      Crawl
                    </Button>
                  ) : null}
                </div>
                {failed ? <p className="mt-2 rounded-input bg-danger-tint px-2.5 py-1.5 text-xs break-words text-ink">{failed}</p> : null}
                {report ? <CrawlSummary report={report} /> : null}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

function CrawlSummary({ report }: { report: CrawlReport }) {
  const objects = Object.entries(report.objects ?? {});
  return (
    <div className="mt-2 text-xs text-ink-2">
      <p className="tnum">
        {plural(report.tables, "table")}, {plural(report.columns, "column")}, {plural(report.foreign_keys, "foreign key")} · {report.upserted} written, {report.unchanged} unchanged
        {report.deleted ? `, ${report.deleted} removed` : ""} · {report.ms} ms
      </p>
      {objects.length ? (
        <ul className="mt-1 flex flex-col gap-0.5">
          {objects.map(([type, s]) => (
            <li key={type} className="tnum">
              <a className="font-mono text-ink underline-offset-2 hover:underline" href={href(["explore", type])}>
                {type}
              </a>
              : {plural(s.rows, "row")} read, {s.upserted} written, {s.unchanged} unchanged, {s.deleted} removed
              {s.invalid ? `, ${s.invalid} invalid` : ""}
              {s.truncated ? " (truncated at the row limit)" : ""}
            </li>
          ))}
        </ul>
      ) : null}
      {Object.entries(report.links ?? {}).length ? (
        <ul className="mt-1 flex flex-col gap-0.5">
          {Object.entries(report.links ?? {}).map(([link, s]) => (
            <li key={link} className="tnum">
              <span className="font-mono text-ink">{link}</span>: {plural(s.rows, "row")} read, {s.linked} linked, {s.unlinked} unlinked
              {s.skipped ? `, ${s.skipped} naming missing objects` : ""}
            </li>
          ))}
        </ul>
      ) : null}
      {(report.notes ?? []).map((n) => (
        <p key={n} className="mt-0.5 text-ink-3">
          {n}
        </p>
      ))}
    </div>
  );
}

function AddConnector({ onClose, onAdded }: { onClose: () => void; onAdded: (name: string) => void }) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [title, setTitle] = useState("");
  const [workspace, setWorkspace] = useState("");
  const [schemas, setSchemas] = useState<string[]>([]);
  const [every, setEvery] = useState(900);
  const [grants, setGrants] = useState(false);
  const add = useMutation({
    mutationFn: () =>
      call("connector.add", {
        name: name.trim(),
        kind: "postgres",
        url: url.trim(),
        title: title.trim() || undefined,
        workspace: workspace.trim() || undefined,
        schemas,
        every_secs: every,
        map_grants: grants,
      }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["connectors"] });
      onAdded(name.trim());
      onClose();
    },
  });
  const nameOk = /^[a-z0-9_-]{1,40}$/.test(name.trim());
  const inline = /^postgres(ql)?:\/\/[^/]*:[^@/]+@/.test(url.trim());
  return (
    <Dialog
      open
      onClose={onClose}
      title="Add a connector"
      footer={
        <>
          {add.error ? <p className="mr-auto self-center text-[13px] text-danger">{errorText(add.error)}</p> : null}
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" busy={add.isPending} disabled={!nameOk || !url.trim()} onClick={() => add.mutate()}>
            Add connector
          </Button>
        </>
      }
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (nameOk && url.trim()) add.mutate();
        }}
      >
        <Field label="Name" hint="Lowercase letters, digits, _ and -; it becomes the source key in entity ids" error={name && !nameOk ? "Use [a-z0-9_-], at most 40" : undefined}>
          {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} placeholder="warehouse" className="font-mono" autoFocus />}
        </Field>
        <Field label="Connection" hint={inline ? "This URL holds a password, which is stored with the connector. env:VAR keeps it out of the store." : "env:VAR reads the URL from the server's environment when crawling (preferred), or a postgres:// URL"}>
          {(id) => <Input id={id} value={url} onChange={(e) => setUrl(e.target.value)} placeholder="env:WAREHOUSE_URL" className="font-mono" spellCheck={false} autoComplete="off" />}
        </Field>
        <Field label="Title">{(id) => <Input id={id} value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Optional" />}</Field>
        <Field label="Schemas" hint="Schemas to crawl, * allowed; empty crawls every schema the role can use">
          {() => <TagInput value={schemas} onChange={setSchemas} placeholder="All schemas" />}
        </Field>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Crawl">
            {(id) => (
              <Select id={id} value={String(every)} onChange={(e) => setEvery(Number(e.target.value))}>
                {EVERY.map((e) => (
                  <option key={e.value} value={e.value}>
                    {e.label}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <Field label="Workspace">{(id) => <Input id={id} value={workspace} onChange={(e) => setWorkspace(e.target.value)} placeholder="Default" />}</Field>
        </div>
        <Checkbox label="Turn table grants into access control (map_grants)" checked={grants} onChange={setGrants} />
        <p className="text-xs text-ink-3">Crawls read metadata in a read-only transaction. Rows are read only for object types whose approved schema says sync.</p>
        <button type="submit" hidden />
      </form>
    </Dialog>
  );
}

// ── review ───────────────────────────────────────────────────────────────

type Tab = "objects" | "links" | "actions" | "skipped";

function Review({
  draft,
  reviewed,
  edits,
  errors,
  setEdits,
  refined,
  instructions,
  setInstructions,
  refining,
  onPlan,
}: {
  draft: OntologyDraft;
  reviewed: OntologyDraft;
  edits: Edits;
  errors: Record<string, string>;
  setEdits: (f: (e: Edits) => Edits) => void;
  refined: Refined | null;
  instructions: string;
  setInstructions: (s: string) => void;
  refining: UseMutationResult<Refined, Error, void>;
  onPlan: () => void;
}) {
  const [tab, setTab] = useState<Tab>("objects");
  const kept = new Set(draft.objects.filter((o) => !edits.objects.has(o.name)).map((o) => o.name));
  const keptLinks = new Set(draft.links.filter((l) => linkKept(l, edits, kept)).map((l) => l.name));
  const toggle = (key: keyof Omit<Edits, "names">, name: string) =>
    setEdits((e) => {
      const next = new Set(e[key]);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return { ...e, [key]: next };
    });
  const invalid = Object.keys(errors).length > 0;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <div className="min-w-0 flex-1">
          <h2 className="text-lg font-semibold tracking-tight">
            Draft of <span className="font-mono">{draft.source}</span>
          </h2>
          <p className="tnum text-[13px] text-ink-2">
            {plural(reviewed.objects.length, "object type")}, {plural(reviewed.links.length, "link type")}, {plural(reviewed.actions.length, "action type")} selected
            {draft.skipped.length ? ` · ${draft.skipped.length} tables not modeled` : ""}
          </p>
        </div>
        <Button variant="primary" icon={<ArrowRight className="size-4" />} disabled={invalid || reviewed.objects.length === 0} title={invalid ? "Fix the type names first" : undefined} onClick={onPlan}>
          Review plan
        </Button>
      </div>

      {draft.notes.length ? (
        <ul className="flex flex-col gap-1 rounded-card bg-warning-tint px-3 py-2 text-[13px] text-ink">
          {draft.notes.map((n) => (
            <li key={n}>{n}</li>
          ))}
        </ul>
      ) : null}

      <div className="rounded-card bg-raised p-3 shadow-card">
        <div className="flex flex-col gap-2 sm:flex-row sm:items-start">
          <Textarea
            aria-label="Instructions for the AI"
            placeholder="Instructions for the AI (optional): use the business names, model refunds as a state…"
            value={instructions}
            onChange={(e) => setInstructions(e.target.value)}
            className="min-h-9 flex-1 py-1.5"
            rows={1}
          />
          <Button icon={<Sparkles className="size-4" />} busy={refining.isPending} onClick={() => refining.mutate()}>
            Refine with AI
          </Button>
        </div>
        <p className="mt-1.5 text-xs text-ink-3">The model suggests renames, labels, types, inverses and transitions; each one is kept only if the draft stays a valid schema change. Refining drafts afresh, so your toggles and renames reset.</p>
        {refining.error ? <p className="mt-2 text-[13px] text-danger">{errorText(refining.error)}</p> : null}
        {refined ? <Suggestions refined={refined} /> : null}
      </div>

      <div className="overflow-x-auto">
        <Segmented<Tab>
          label="Draft section"
          value={tab}
          onChange={setTab}
          options={[
            { value: "objects", label: `Types ${reviewed.objects.length}/${draft.objects.length}` },
            { value: "links", label: `Links ${reviewed.links.length}/${draft.links.length}` },
            { value: "actions", label: `Actions ${reviewed.actions.length}/${draft.actions.length}` },
            { value: "skipped", label: `Skipped ${draft.skipped.length}` },
          ]}
        />
      </div>

      {tab === "objects" ? (
        draft.objects.length === 0 ? (
          <EmptyState title="No object types">No table qualified; see what was not modeled and why.</EmptyState>
        ) : (
          <ul className="flex flex-col gap-3">
            {draft.objects.map((o) => (
              <ObjectCard
                key={o.name}
                o={o}
                open={draft.objects.length <= 4}
                included={!edits.objects.has(o.name)}
                name={edits.names[o.name] ?? o.name}
                error={errors[o.name]}
                excludedProps={edits.props}
                onToggle={() => toggle("objects", o.name)}
                onToggleProp={(p) => toggle("props", `${o.name}.${p}`)}
                onRename={(n) => setEdits((e) => ({ ...e, names: { ...e.names, [o.name]: n } }))}
              />
            ))}
          </ul>
        )
      ) : tab === "links" ? (
        <LinkTable links={draft.links} edits={edits} kept={kept} names={edits.names} onToggle={(n) => toggle("links", n)} />
      ) : tab === "actions" ? (
        <ActionTable actions={draft.actions} edits={edits} kept={kept} keptLinks={keptLinks} onToggle={(n) => toggle("actions", n)} />
      ) : draft.skipped.length === 0 ? (
        <EmptyState title="Every table was modeled" />
      ) : (
        <ul className="divide-y divide-hairline rounded-card bg-raised shadow-card">
          {draft.skipped.map((s) => (
            <li key={s.table} className="flex flex-wrap gap-x-3 px-4 py-2.5 text-[13px]">
              <span className="font-mono text-ink">{s.table}</span>
              <span className="text-ink-2">{s.reason}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Suggestions({ refined }: { refined: Refined }) {
  const rejected = new Set(refined.rejected.map((r) => JSON.stringify(r.suggestion)));
  const accepted = refined.suggestions.filter((s) => !rejected.has(JSON.stringify(s)));
  if (refined.llm === "none") return null;
  return (
    <div className="mt-3 border-t border-hairline pt-3">
      <p className="flex flex-wrap items-center gap-2 text-[13px]">
        <Bot className="size-4 text-ink-3" aria-hidden />
        <span className="font-medium">{refined.applied} of {plural(refined.suggestions.length, "suggestion")} kept</span>
        <span className="tnum text-xs text-ink-3">
          {refined.llm} · {refined.tokens.toLocaleString()} tokens
        </span>
      </p>
      {accepted.length ? (
        <ul className="mt-2 flex flex-col gap-1.5">
          {accepted.map((s, i) => (
            <SuggestionRow key={`a${i}`} s={s} ok />
          ))}
        </ul>
      ) : null}
      {refined.rejected.length ? (
        <>
          <p className="mt-3 text-xs font-medium text-ink-2">Not kept</p>
          <ul className="mt-1 flex flex-col gap-1.5">
            {refined.rejected.map((r, i) => (
              <SuggestionRow key={`r${i}`} s={r.suggestion} error={r.error} />
            ))}
          </ul>
        </>
      ) : null}
    </div>
  );
}

function SuggestionRow({ s, ok, error }: { s: Suggestion; ok?: boolean; error?: string }) {
  return (
    <li className="flex items-start gap-2 text-[13px]">
      {ok ? <CircleCheck className="mt-0.5 size-3.5 shrink-0 text-success" aria-label="Kept" /> : <CircleAlert className="mt-0.5 size-3.5 shrink-0 text-danger" aria-label="Not kept" />}
      <div className="min-w-0">
        <p className="break-words">
          <span className="font-mono text-ink">{s.target}</span> <span className="text-ink-3">{s.field}</span> <span className="font-mono text-ink-2 line-through decoration-ink-3">{display(s.from)}</span>{" "}
          <span className="text-ink-3">→</span> <span className="font-mono text-ink">{display(s.to)}</span>
        </p>
        {s.why ? <p className="text-xs text-ink-2">{s.why}</p> : null}
        {error ? <p className="text-xs text-danger">{error}</p> : null}
      </div>
    </li>
  );
}

function ObjectCard({
  o,
  open: initiallyOpen,
  included,
  name,
  error,
  excludedProps,
  onToggle,
  onToggleProp,
  onRename,
}: {
  o: DraftObject;
  open: boolean;
  included: boolean;
  name: string;
  error?: string;
  excludedProps: Set<string>;
  onToggle: () => void;
  onToggleProp: (p: string) => void;
  onRename: (n: string) => void;
}) {
  const [open, setOpen] = useState(initiallyOpen);
  const bodyId = `obj-${o.name}`;
  return (
    <li className={cx("rounded-card bg-raised shadow-card transition-opacity duration-150", !included && "opacity-60")}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-3">
        <input type="checkbox" checked={included} onChange={onToggle} aria-label={`Include object type ${o.name}`} className="size-4 rounded accent-[var(--accent)]" />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <Input
              aria-label={`Name of the type from ${o.table}`}
              aria-invalid={!!error}
              value={name}
              disabled={!included}
              onChange={(e) => onRename(e.target.value)}
              className={cx("h-8 w-48 font-mono text-[13px]", error && "outline-2 outline-danger")}
              spellCheck={false}
            />
            <span className="text-[13px] text-ink-2">{o.label}</span>
            <Badge>{o.category}</Badge>
            {o.sync ? <Badge tone="accent">sync</Badge> : null}
          </div>
          {error ? <p className="mt-1 text-xs text-danger">{error}</p> : null}
          <p className="mt-1 truncate text-xs text-ink-3">
            <span className="font-mono text-ink-2">{o.table}</span> · key <span className="font-mono">{o.key.join(", ")}</span> · title <span className="font-mono">{o.title ?? "id"}</span>
            {o.state ? (
              <>
                {" "}
                · state <span className="font-mono">{o.state}</span> ({o.states.join(" | ")})
              </>
            ) : null}
          </p>
        </div>
        <span className="tnum text-xs text-ink-3" title="Confidence">
          {Math.round(o.confidence * 100)}%
        </span>
        <button
          aria-expanded={open}
          aria-controls={bodyId}
          onClick={() => setOpen((v) => !v)}
          className="inline-flex h-8 items-center gap-1 rounded-input px-2 text-[13px] text-ink-2 hover:bg-sunken hover:text-ink"
        >
          {plural(o.properties.length, "property", "properties")}
          <ChevronRight className={cx("size-4 transition-transform duration-150", open && "rotate-90")} aria-hidden />
        </button>
      </div>
      {open ? (
        <div id={bodyId} className="border-t border-hairline px-4 pt-2 pb-3">
          {o.description ? <p className="mb-2 text-[13px] text-ink-2">{o.description}</p> : null}
          <ul className="mb-3 flex flex-col gap-0.5 text-xs text-ink-3">
            {o.reasons.map((r) => (
              <li key={r}>· {r}</li>
            ))}
          </ul>
          {o.properties.length === 0 ? (
            <p className="text-[13px] text-ink-3">No properties besides the key and links.</p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full min-w-[560px] border-collapse text-[13px]">
                <thead className="text-left text-xs text-ink-3">
                  <tr className="shadow-[0_1px_0_var(--hairline)]">
                    <th className="w-8 py-1.5 font-medium">
                      <span className="sr-only">Include</span>
                    </th>
                    <th className="py-1.5 pr-3 font-medium">Property</th>
                    <th className="py-1.5 pr-3 font-medium">Column</th>
                    <th className="py-1.5 pr-3 font-medium">Type</th>
                    <th className="py-1.5 font-medium">Why</th>
                  </tr>
                </thead>
                <tbody>
                  {o.properties.map((p) => {
                    const on = !excludedProps.has(`${o.name}.${p.name}`);
                    return (
                      <tr key={p.name} className={cx("border-b border-hairline last:border-0", !on && "opacity-50")}>
                        <td className="py-1.5 align-top">
                          <input type="checkbox" checked={on} disabled={!included} onChange={() => onToggleProp(p.name)} aria-label={`Include property ${p.name}`} className="mt-0.5 size-4 rounded accent-[var(--accent)]" />
                        </td>
                        <td className="py-1.5 pr-3 align-top">
                          <span className="font-mono text-ink">{p.name}</span>
                          <span className="ml-1.5 inline-flex gap-1 align-middle">
                            {p.required ? <Badge>required</Badge> : null}
                            {p.unique ? <Badge>unique</Badge> : null}
                            {p.searchable ? <Badge>searchable</Badge> : null}
                          </span>
                          {p.description ? <p className="text-xs text-ink-3">{p.description}</p> : null}
                        </td>
                        <td className="py-1.5 pr-3 align-top font-mono text-xs text-ink-2">
                          {p.column}
                          <span className="block text-ink-3">{p.data_type}</span>
                        </td>
                        <td className="py-1.5 pr-3 align-top">
                          <TypeBadge ty={p.ty} />
                          {p.values.length ? <p className="mt-0.5 max-w-[180px] text-xs break-words text-ink-3">{p.values.join(" | ")}</p> : null}
                        </td>
                        <td className="py-1.5 align-top text-xs text-ink-2">{p.reason}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
      ) : null}
    </li>
  );
}

function LinkTable({ links, edits, kept, names, onToggle }: { links: DraftLink[]; edits: Edits; kept: Set<string>; names: Record<string, string>; onToggle: (n: string) => void }) {
  if (links.length === 0) return <EmptyState title="No link types">No foreign key, join table or learned join connects the drafted tables.</EmptyState>;
  const rn = (n: string) => names[n]?.trim() || n;
  return (
    <div className="overflow-x-auto rounded-card bg-raised shadow-card">
      <table className="w-full min-w-[720px] border-collapse text-[13px]">
        <thead className="text-left text-xs text-ink-3">
          <tr className="shadow-[0_1px_0_var(--hairline)]">
            <th className="w-10 px-4 py-2 font-medium">
              <span className="sr-only">Include</span>
            </th>
            <th className="py-2 pr-3 font-medium">Link type</th>
            <th className="py-2 pr-3 font-medium">From → to</th>
            <th className="py-2 pr-3 font-medium">Via</th>
            <th className="py-2 pr-3 text-right font-medium">Confidence</th>
            <th className="py-2 pr-4 font-medium">Why</th>
          </tr>
        </thead>
        <tbody>
          {links.map((l) => {
            const ends = kept.has(l.from) && kept.has(l.to);
            const on = ends && !edits.links.has(l.name);
            return (
              <tr key={l.name} className={cx("border-b border-hairline last:border-0", !on && "opacity-50")}>
                <td className="px-4 py-2 align-top">
                  <input
                    type="checkbox"
                    checked={on}
                    disabled={!ends}
                    title={ends ? undefined : "An object type it connects is left out"}
                    onChange={() => onToggle(l.name)}
                    aria-label={`Include link type ${l.name}`}
                    className="mt-0.5 size-4 rounded accent-[var(--accent)]"
                  />
                </td>
                <td className="py-2 pr-3 align-top">
                  <span className="font-mono text-ink">{l.name}</span>
                  {l.inverse ? <span className="block text-xs text-ink-3">inverse {l.inverse}</span> : null}
                </td>
                <td className="py-2 pr-3 align-top whitespace-nowrap">
                  <span className="font-mono">{rn(l.from)}</span> <span className="text-ink-3">→</span> <span className="font-mono">{rn(l.to)}</span>
                  <span className="block text-xs text-ink-3">{l.cardinality === "one" ? "each has one" : "each has many"}</span>
                </td>
                <td className="py-2 pr-3 align-top">
                  <Badge tone={l.via === "learned" ? "warning" : "neutral"}>{l.via.replace("_", " ")}</Badge>
                  <span className="block font-mono text-xs text-ink-3">{l.table ? `${l.table}` : l.column}</span>
                </td>
                <td className="tnum py-2 pr-3 text-right align-top text-ink-2">{Math.round(l.confidence * 100)}%</td>
                <td className="py-2 pr-4 align-top text-xs text-ink-2">{l.reason}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

const RISK_TONE = { low: "neutral", medium: "warning", high: "danger" } as const;

function ActionTable({ actions, edits, kept, keptLinks, onToggle }: { actions: DraftAction[]; edits: Edits; kept: Set<string>; keptLinks: Set<string>; onToggle: (n: string) => void }) {
  if (actions.length === 0) return <EmptyState title="No action types">Draft with action types on to get create, edit, transition and relink actions.</EmptyState>;
  return (
    <div className="overflow-x-auto rounded-card bg-raised shadow-card">
      <table className="w-full min-w-[640px] border-collapse text-[13px]">
        <thead className="text-left text-xs text-ink-3">
          <tr className="shadow-[0_1px_0_var(--hairline)]">
            <th className="w-10 px-4 py-2 font-medium">
              <span className="sr-only">Include</span>
            </th>
            <th className="py-2 pr-3 font-medium">Action type</th>
            <th className="py-2 pr-3 font-medium">Kind</th>
            <th className="py-2 pr-3 font-medium">Risk</th>
            <th className="py-2 pr-4 font-medium">Why</th>
          </tr>
        </thead>
        <tbody>
          {actions.map((a) => {
            const possible = kept.has(a.object) && (a.kind !== "relink" || !a.link || keptLinks.has(a.link));
            const on = possible && !edits.actions.has(a.name);
            return (
              <tr key={a.name} className={cx("border-b border-hairline last:border-0", !on && "opacity-50")}>
                <td className="px-4 py-2 align-top">
                  <input
                    type="checkbox"
                    checked={on}
                    disabled={!possible}
                    title={possible ? undefined : "Its object or link type is left out"}
                    onChange={() => onToggle(a.name)}
                    aria-label={`Include action type ${a.name}`}
                    className="mt-0.5 size-4 rounded accent-[var(--accent)]"
                  />
                </td>
                <td className="py-2 pr-3 align-top">
                  <span className="font-mono text-ink">{a.name}</span>
                  {a.description ? <span className="block text-xs text-ink-3">{a.description}</span> : null}
                </td>
                <td className="py-2 pr-3 align-top whitespace-nowrap text-ink-2">
                  {a.kind}
                  {a.state ? <span className="text-ink-3"> → {a.state}</span> : null}
                  {a.link ? <span className="text-ink-3"> {a.link}</span> : null}
                </td>
                <td className="py-2 pr-3 align-top">
                  <Badge tone={RISK_TONE[a.risk]}>{a.risk}</Badge>
                </td>
                <td className="py-2 pr-4 align-top text-xs text-ink-2">{a.reason}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

// ── plan, apply, propose ─────────────────────────────────────────────────

function PlanDialog({ draft, canApply, canPropose, onClose, onApplied, onProposed }: { draft: OntologyDraft; canApply: boolean; canPropose: boolean; onClose: () => void; onApplied: (version: number) => void; onProposed: (id: string) => void }) {
  const [mode, setMode] = useState<"review" | "propose">("review");
  const [title, setTitle] = useState(`Ontology for data source ${draft.source}`);
  const [rationale, setRationale] = useState("");
  const body = JSON.stringify(draft);
  // The server rebuilds the change set from the reviewed parts; the plan is
  // the same `schema.plan` every schema change goes through.
  const plan = useQuery<{ changes: OntologyDraft["changes"]; plan: SchemaPlan }>({
    queryKey: ["build-plan", body],
    queryFn: async () => {
      const preview = await call<{ draft: OntologyDraft }>("ontology.apply_draft", { draft, preview: true });
      const changes = preview.draft.changes;
      return { changes, plan: await call<SchemaPlan>("schema.plan", changes) };
    },
    staleTime: 0,
  });
  const apply = useMutation({
    mutationFn: () => call<{ version: number | { version: number } }>("ontology.apply_draft", { draft, mode: "apply", no_crawl: true }),
    onSuccess: (r) => onApplied(typeof r.version === "object" ? r.version.version : r.version),
  });
  const propose = useMutation({
    mutationFn: () => call<{ proposal: { id: string } }>("ontology.apply_draft", { draft, mode: "propose", title, rationale: rationale.trim() || undefined, proposal_source: "ui" }),
    onSuccess: (r) => onProposed(r.proposal.id),
  });
  const p = plan.data?.plan;
  const applicable = !!p && p.valid && !(p.needs_migration ?? []).length;
  const err = apply.error ?? propose.error;
  return (
    <Dialog
      open
      wide
      onClose={onClose}
      title={mode === "review" ? "Plan for the reviewed draft" : "File a proposal"}
      footer={
        mode === "review" ? (
          <>
            {err ? <p className="mr-auto self-center text-[13px] text-danger">{errorText(err)}</p> : null}
            <Button variant="ghost" onClick={onClose}>
              Keep reviewing
            </Button>
            {canPropose ? (
              <Button disabled={!p?.valid} onClick={() => setMode("propose")}>
                Propose for review
              </Button>
            ) : null}
            {canApply ? (
              <Button variant="primary" busy={apply.isPending} disabled={!applicable} onClick={() => apply.mutate()}>
                Apply as version {(p?.base_version ?? 0) + 1}
              </Button>
            ) : null}
          </>
        ) : (
          <>
            {err ? <p className="mr-auto self-center text-[13px] text-danger">{errorText(err)}</p> : null}
            <Button variant="ghost" onClick={() => setMode("review")}>
              Back
            </Button>
            <Button variant="primary" busy={propose.isPending} disabled={!title.trim()} onClick={() => propose.mutate()}>
              File proposal
            </Button>
          </>
        )
      }
    >
      {mode === "review" ? (
        <div className="flex flex-col gap-4">
          <p className="tnum text-[13px] text-ink-2">
            {plural(draft.objects.length, "object type")}, {plural(draft.links.length, "link type")} and {plural(draft.actions.length, "action type")} from {draft.source}
            {plan.data ? ` · ${plural(plan.data.changes.changes.length, "change")}` : ""}.
          </p>
          {plan.isLoading ? <Spinner label="Working out the impact" /> : plan.error ? <ErrorState error={plan.error} retry={() => plan.refetch()} /> : p ? <PlanView plan={p} /> : null}
          {!canApply && canPropose ? <p className="text-[13px] text-ink-2">You can propose this ontology; an approver applies it.</p> : null}
          {!canApply && !canPropose ? <p className="text-[13px] text-ink-2">You may neither apply nor propose schema changes.</p> : null}
        </div>
      ) : (
        <div className="flex flex-col gap-4">
          <Field label="Title">{(id) => <Input id={id} value={title} onChange={(e) => setTitle(e.target.value)} autoFocus />}</Field>
          <Field label="Rationale" hint="Why these types, and the evidence; left empty, it lists what was drafted">
            {(id) => <Textarea id={id} value={rationale} onChange={(e) => setRationale(e.target.value)} />}
          </Field>
          {p ? <PlanView plan={p} /> : null}
        </div>
      )}
    </Dialog>
  );
}

function AppliedBanner({ applied, connector, canCrawl, crawl, onDismiss }: { applied: { version: number; source: string; synced: boolean }; connector: boolean; canCrawl: boolean; crawl: CrawlMutation; onDismiss: () => void }) {
  const done = crawl.data?.crawls.find((r) => r.connector === applied.source);
  return (
    <div role="status" className="mb-4 rounded-card bg-accent-tint p-4">
      <p className="flex items-center gap-2 text-[13px] font-medium text-ink">
        <CircleCheck className="size-4 text-success" aria-hidden /> Applied as schema version {applied.version}.
      </p>
      <p className="mt-1 text-[13px] text-ink-2">
        {!applied.synced
          ? "No type syncs rows; objects come from actions and ingestion."
          : done
            ? "Rows are synced into objects."
            : !connector
              ? `No connector named ${applied.source} is registered here, so no rows are read: register one with that name (onto connect) and crawl it to sync objects.`
              : canCrawl
                ? `Rows are not synced yet: crawl ${applied.source} to read them into objects.`
                : `Rows are synced when ${applied.source} is next crawled (onto crawl ${applied.source}).`}
      </p>
      <div className="mt-3 flex flex-wrap items-center gap-2">
        {applied.synced && canCrawl && !done ? (
          <Button size="sm" variant="primary" icon={<RefreshCw className="size-3.5" />} busy={crawl.isPending} onClick={() => crawl.mutate(applied.source)}>
            Crawl now
          </Button>
        ) : null}
        <a href={href(["explore"])} className="inline-flex h-8 items-center rounded-input px-2.5 text-[13px] font-medium text-accent hover:bg-raised">
          Open the Explorer
        </a>
        <Button size="sm" variant="ghost" onClick={onDismiss}>
          Dismiss
        </Button>
      </div>
      {crawl.error ? <p className="mt-2 text-[13px] text-danger">{errorText(crawl.error)}</p> : null}
      {done ? <CrawlSummary report={done} /> : null}
    </div>
  );
}

// ── advisor ──────────────────────────────────────────────────────────────

function Advisor({ source }: { source: string }) {
  const [question, setQuestion] = useState("");
  const ask = useMutation({ mutationFn: (q: string) => call<Advice>("ontology.advise", { question: q, source }) });
  const examples = ["Which link types should orders have?", "What property types do the columns get?", "Which actions can people run?"];
  const submit = (q: string) => {
    setQuestion(q);
    if (q.trim()) ask.mutate(q.trim());
  };
  return (
    <section aria-labelledby="advisor-heading" className="rounded-card bg-raised p-4 shadow-card">
      <h2 id="advisor-heading" className="flex items-center gap-2 text-[13px] font-semibold">
        <MessageSquareText className="size-4 text-ink-3" aria-hidden /> Ask the modeling advisor
      </h2>
      <p className="mt-0.5 text-xs text-ink-3">Answers from the draft of {source}; questions that need reasoning go to the model when one is configured.</p>
      <form
        className="mt-3 flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          submit(question);
        }}
      >
        <Input aria-label="Question for the advisor" value={question} onChange={(e) => setQuestion(e.target.value)} placeholder="How should I model the sales tables?" />
        <Button type="submit" busy={ask.isPending} disabled={!question.trim()}>
          Ask
        </Button>
      </form>
      {!ask.data && !ask.isPending ? (
        <div className="mt-2 flex flex-wrap gap-1.5">
          {examples.map((q) => (
            <button key={q} onClick={() => submit(q)} className="rounded-full bg-sunken px-2.5 py-1 text-left text-xs text-ink-2 hover:text-ink">
              {q}
            </button>
          ))}
        </div>
      ) : null}
      {ask.error ? <p className="mt-3 text-[13px] text-danger">{errorText(ask.error)}</p> : null}
      {ask.data ? (
        <Section title="Answer" aside={<Badge tone={ask.data.mode === "llm" ? "accent" : "neutral"}>{ask.data.mode === "llm" ? `model · ${ask.data.tokens} tokens` : "from the draft"}</Badge>}>
          <p className="text-[13px] leading-relaxed whitespace-pre-wrap text-ink">{ask.data.answer}</p>
          {ask.data.note ? <p className="mt-1 text-xs text-ink-3">{ask.data.note}</p> : null}
          {ask.data.facts.length ? (
            <details className="mt-2 text-xs text-ink-2">
              <summary className="cursor-pointer text-ink-3">{plural(ask.data.facts.length, "fact")} behind it</summary>
              <ul className="mt-1 flex flex-col gap-0.5">
                {ask.data.facts.map((f) => (
                  <li key={f}>· {f}</li>
                ))}
              </ul>
            </details>
          ) : null}
        </Section>
      ) : null}
    </section>
  );
}
