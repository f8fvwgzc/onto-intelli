import { useEffect, useMemo, useRef, useState } from "react";
import { keepPreviousData, useQueries, useQuery } from "@tanstack/react-query";
import { ArrowDown, ArrowUp, ChevronLeft, ChevronRight, Columns3, Database, Plus, SlidersHorizontal, X, Zap } from "lucide-react";

import { ActionDialog, actionsFor } from "../components/ActionDialog";
import { StateBadge } from "../components/EntityCard";
import { PageHeader } from "../components/Shell";
import { Badge, Button, Checkbox, Dot, EmptyState, ErrorState, IconButton, Input, Select, Skeleton, Spinner, cx } from "../components/ui";
import { call } from "../lib/api";
import { ago, display, kindColor, kindLabel, plural, when } from "../lib/format";
import { useSchema, useScope } from "../lib/queries";
import { href, navigate, setQuery, useRoute } from "../lib/router";
import { useSession } from "../lib/session";
import type { Entity, ExploreResult, Filter, FilterOp, Json, ObjectTypeSummary, PropType, Stats } from "../lib/types";

const PAGE = 50;
const CATEGORIES = ["actor", "container", "thing", "artifact", "knowledge"];

export function ExplorePage() {
  const route = useRoute();
  const kind = route.path[1];
  return kind ? <ObjectTable kind={kind} /> : <TypeList />;
}

export function useObjectTypes() {
  const scope = useScope();
  const session = useSession();
  return useQuery<{ types: ObjectTypeSummary[] }>({
    queryKey: ["object-types", ...scope],
    queryFn: () => call("objects.types", { workspace: session.workspace || undefined }),
  });
}

// ── object types ─────────────────────────────────────────────────────────

function TypeList() {
  const types = useObjectTypes();
  const schema = useSchema().data;
  const [filter, setFilter] = useState("");
  const [only, setOnly] = useState(false);
  const groups = useMemo(() => {
    const f = filter.trim().toLowerCase();
    const list = (types.data?.types ?? []).filter(
      (t) => (!f || t.name.includes(f.replace(/\s+/g, "_")) || t.label.toLowerCase().includes(f) || t.backing?.table.includes(f)) && (!only || t.count > 0),
    );
    const order = (c: string) => (CATEGORIES.indexOf(c) < 0 ? CATEGORIES.length : CATEGORIES.indexOf(c));
    const by = new Map<string, ObjectTypeSummary[]>();
    for (const t of list) by.set(t.category, [...(by.get(t.category) ?? []), t]);
    return [...by.entries()].sort((a, b) => order(a[0]) - order(b[0])).map(([c, l]) => [c, l.sort((a, b) => b.count - a.count || a.label.localeCompare(b.label))] as const);
  }, [types.data, filter, only]);
  const total = (types.data?.types ?? []).reduce((n, t) => n + t.count, 0);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title="Explorer" context={types.data ? `${plural(types.data.types.length, "object type")} · ${plural(total, "object")} you can see` : "Object types and their objects"}>
        <Input aria-label="Filter object types" placeholder="Filter types" value={filter} onChange={(e) => setFilter(e.target.value)} className="h-8 w-40 sm:w-56" />
        <Checkbox label="With objects" checked={only} onChange={setOnly} />
      </PageHeader>
      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4 md:px-5">
        {types.isLoading ? (
          <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
            {Array.from({ length: 9 }, (_, i) => (
              <Skeleton key={i} className="h-[92px]" />
            ))}
          </div>
        ) : types.error ? (
          <ErrorState error={types.error} retry={() => types.refetch()} />
        ) : groups.length === 0 ? (
          <EmptyState title="No object types match" action={<a className="text-[13px] text-accent underline-offset-2 hover:underline" href={href(["build"])}>Build them from a data source</a>}>
            Object types come from the schema; the builder drafts them from a connected database.
          </EmptyState>
        ) : (
          groups.map(([category, list]) => (
            <section key={category} className="mb-6">
              <h2 className="mb-2 text-xs font-medium tracking-wide text-ink-3 uppercase">{category}</h2>
              <ul className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
                {list.map((t) => (
                  <li key={t.name}>
                    <a
                      href={href(["explore", t.name])}
                      className="flex h-full flex-col gap-1 rounded-card bg-raised p-3 shadow-card transition-shadow duration-150 hover:shadow-[0_0_0_1px_var(--accent)]"
                    >
                      <span className="flex items-center gap-2">
                        <Dot color={kindColor(t.name, schema)} />
                        <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-ink">{t.label}</span>
                        <span className="tnum text-[13px] text-ink-2">{t.count.toLocaleString()}</span>
                      </span>
                      <span className="line-clamp-1 text-xs text-ink-3">
                        <span className="font-mono">{t.name}</span>
                        {t.description ? ` · ${t.description}` : ""}
                      </span>
                      <span className="mt-auto flex flex-wrap items-center gap-x-2 gap-y-1 pt-1 text-xs text-ink-3">
                        {t.backing ? (
                          <span className="inline-flex min-w-0 items-center gap-1 font-mono text-ink-2" title={`Backed by ${t.backing.source} ${t.backing.table}`}>
                            <Database className="size-3 shrink-0" aria-hidden />
                            <span className="truncate">
                              {t.backing.source}.{t.backing.table}
                            </span>
                          </span>
                        ) : null}
                        <span>{plural(t.properties, "property", "properties")}</span>
                        <span>{plural(t.links.length, "link")}</span>
                        {t.actions.length ? <span>{plural(t.actions.length, "action")}</span> : null}
                      </span>
                    </a>
                  </li>
                ))}
              </ul>
            </section>
          ))
        )}
      </div>
    </div>
  );
}

// ── objects of one type ──────────────────────────────────────────────────

const OP_LABEL: Record<FilterOp, string> = {
  eq: "is",
  ne: "is not",
  in: "is any of",
  gt: ">",
  gte: "≥",
  lt: "<",
  lte: "≤",
  contains: "contains",
  exists: "is set",
  missing: "is not set",
};

function readFilters(raw: string | null): Filter[] {
  if (!raw) return [];
  try {
    const v = JSON.parse(raw);
    return Array.isArray(v) ? v.filter((f) => f && typeof f.prop === "string" && typeof f.op === "string") : [];
  } catch {
    return [];
  }
}

function writeFilters(filters: Filter[]) {
  setQuery({ f: filters.length ? JSON.stringify(filters) : undefined, page: undefined });
}

const COLS_KEY = "onto.explore.columns";

function savedColumns(kind: string): string[] | null {
  try {
    const all = JSON.parse(localStorage.getItem(COLS_KEY) ?? "{}");
    return Array.isArray(all[kind]) ? all[kind] : null;
  } catch {
    return null;
  }
}

function saveColumns(kind: string, cols: string[]) {
  try {
    const all = JSON.parse(localStorage.getItem(COLS_KEY) ?? "{}");
    all[kind] = cols;
    localStorage.setItem(COLS_KEY, JSON.stringify(all));
  } catch {
    // Storage blocked: the choice lasts until the page reloads.
  }
}

function ObjectTable({ kind }: { kind: string }) {
  const route = useRoute();
  const scope = useScope();
  const session = useSession();
  const schema = useSchema().data;
  const types = useObjectTypes().data?.types;
  const summary = types?.find((t) => t.name === kind);
  const q = route.query.get("q") ?? "";
  const sort = route.query.get("sort") ?? "";
  const page = Number(route.query.get("page") ?? 0);
  const rawFilters = route.query.get("f");
  const filters = useMemo(() => readFilters(rawFilters), [rawFilters]);
  const workspace = session.workspace || undefined;
  const [text, setText] = useState(q);
  const [showFacets, setShowFacets] = useState(false);
  const [acting, setActing] = useState<{ actions: string[]; item?: Entity } | null>(null);
  // A different type starts from its own search text.
  const [seenKind, setSeenKind] = useState(kind);
  if (seenKind !== kind) {
    setSeenKind(kind);
    setText(q);
  }
  useEffect(() => {
    const t = window.setTimeout(() => text !== q && setQuery({ q: text || undefined, page: undefined }), 250);
    return () => window.clearTimeout(t);
  }, [text, q]);

  const spec = { kind, workspace, search: q || undefined, filters, sort: sort || undefined, limit: PAGE, offset: page * PAGE };
  const data = useQuery<ExploreResult>({
    queryKey: ["explore", JSON.stringify(spec), ...scope],
    queryFn: () => call<ExploreResult>("objects.explore", spec),
    placeholderData: (prev, prevQuery) => (prevQuery?.queryKey[1] && JSON.parse(prevQuery.queryKey[1] as string).kind === kind ? prev : undefined),
  });

  // A facet with a filter of its own counts over the set the other filters
  // leave, so its other values stay visible and pickable.
  const filtered = [...new Set(filters.filter((f) => f.op === "eq" || f.op === "in").map((f) => f.prop))];
  const own = useQueries({
    queries: filtered.map((prop) => {
      const others = { kind, workspace, search: q || undefined, filters: filters.filter((f) => f.prop !== prop), limit: 1, facets: [prop], stats: [] };
      return {
        queryKey: ["explore", JSON.stringify(others), ...scope],
        queryFn: () => call<ExploreResult>("objects.explore", others),
        placeholderData: keepPreviousData,
      };
    }),
  });
  const facets = useMemo(() => {
    const out = { ...(data.data?.facets ?? {}) };
    filtered.forEach((prop, i) => {
      const f = own[i]?.data?.facets?.[prop];
      if (f) out[prop] = f;
    });
    return out;
  }, [data.data, own, filtered]);

  const r = data.data;
  const allCols = r?.columns ?? [];
  const [chosen, setChosen] = useState<string[] | null>(() => savedColumns(kind));
  useEffect(() => setChosen(savedColumns(kind)), [kind]);
  const visible = chosen ? allCols.filter((c) => chosen.includes(c.name)) : allCols.slice(0, 6);
  const pickColumns = (cols: string[]) => {
    setChosen(cols);
    saveColumns(kind, cols);
  };
  const { acting: rowActions, creating } = useMemo(() => {
    const fromSchema = actionsFor(kind, schema?.action_types);
    const listed = new Set([...(r?.actions ?? []), ...fromSchema.acting]);
    return { acting: [...listed].sort(), creating: fromSchema.creating };
  }, [kind, schema, r?.actions]);

  const total = r?.total ?? 0;
  const pages = Math.max(1, Math.ceil(total / PAGE));
  const label = r?.label ?? summary?.label ?? kindLabel(kind, schema);
  const tyOf = (prop: string): PropType | undefined => allCols.find((c) => c.name === prop)?.ty;
  // The title and state columns are named _title and _state when the type
  // has a property called title or state.
  const metaOf = (col: string): "title" | "state" | undefined => {
    const bare = col.replace(/^_/, "");
    if (bare !== "title" && bare !== "state") return undefined;
    return col.startsWith("_") || !allCols.some((c) => c.name === `_${bare}`) ? bare : undefined;
  };

  const toggleFacet = (prop: string, value: Json) => {
    const current = filters.find((f) => f.prop === prop && (f.op === "eq" || f.op === "in"));
    const picked: Json[] = current ? (current.op === "in" && Array.isArray(current.value) ? current.value : [current.value ?? null]) : [];
    const has = picked.some((v) => JSON.stringify(v) === JSON.stringify(value));
    const next = has ? picked.filter((v) => JSON.stringify(v) !== JSON.stringify(value)) : [...picked, value];
    const rest = filters.filter((f) => f !== current);
    writeFilters(next.length === 0 ? rest : [...rest, next.length === 1 ? { prop, op: "eq", value: next[0] } : { prop, op: "in", value: next }]);
  };
  const setRange = (prop: string, lo: number, hi: number) => {
    const rest = filters.filter((f) => !(f.prop === prop && ["gt", "gte", "lt", "lte"].includes(f.op)));
    writeFilters([...rest, { prop, op: "gte", value: lo }, { prop, op: "lte", value: hi }]);
  };
  const sortBy = (col: string) => {
    const next = sort === col ? `-${col}` : sort === `-${col}` ? undefined : col;
    setQuery({ sort: next, page: undefined });
  };

  const facetCount = Object.keys(facets).length + Object.keys(r?.stats ?? {}).length;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title={label}
        context={
          <>
            {r ? `${plural(total, "object")}${q ? ` matching “${q}”` : ""}` : kind}
            {summary?.backing ? ` · backed by ${summary.backing.source}.${summary.backing.table}` : ""}
          </>
        }
      >
        <Input aria-label={`Search ${label}`} placeholder="Search" value={text} onChange={(e) => setText(e.target.value)} className="h-8 w-36 sm:w-56" />
        <Button size="sm" variant="ghost" className="md:hidden" icon={<SlidersHorizontal className="size-4" />} aria-expanded={showFacets} onClick={() => setShowFacets((s) => !s)}>
          Filters{filters.length ? ` (${filters.length})` : ""}
        </Button>
        <ColumnChooser columns={allCols} visible={visible.map((c) => c.name)} onChange={pickColumns} />
        {creating.length ? (
          <Button size="sm" variant="primary" icon={<Plus className="size-4" />} onClick={() => setActing({ actions: creating })}>
            New
          </Button>
        ) : null}
      </PageHeader>

      <div className="flex min-h-0 flex-1 flex-col md:flex-row">
        <aside
          aria-label="Facets and statistics"
          className={cx(
            "min-h-0 shrink-0 overflow-y-auto border-hairline px-4 py-3 md:block md:w-[264px] md:border-r",
            showFacets ? "block max-h-[45dvh] border-b md:max-h-none md:border-b-0" : "hidden",
          )}
        >
          <a href={href(["explore"])} className="text-[13px] text-ink-2">
            ← Object types
          </a>
          {data.isLoading ? (
            <div className="mt-4 flex flex-col gap-2">
              <Skeleton className="h-4 w-1/2" />
              <Skeleton className="h-20 w-full" />
            </div>
          ) : facetCount === 0 && r ? (
            <p className="mt-4 text-[13px] text-ink-3">No facets: the objects have no state, enum or few-valued properties.</p>
          ) : null}
          {Object.entries(facets).map(([prop, values]) => (
            <FacetGroup
              key={prop}
              prop={prop}
              label={allCols.find((c) => c.name === prop)?.label ?? kindLabel(prop)}
              values={values}
              filters={filters}
              onToggle={(v) => toggleFacet(prop, v)}
            />
          ))}
          {Object.entries(r?.stats ?? {}).map(([prop, s]) => (
            <StatsGroup key={prop} prop={prop} label={allCols.find((c) => c.name === prop)?.label ?? kindLabel(prop)} stats={s} timestamp={tyOf(prop) === "timestamp" || prop.endsWith("_at")} onRange={(lo, hi) => setRange(prop, lo, hi)} />
          ))}
          {r?.links.length ? (
            <div className="mt-5">
              <h3 className="mb-1 text-[13px] font-semibold">Links</h3>
              <ul className="flex flex-col gap-0.5 text-xs text-ink-2">
                {r.links.map((l) => (
                  <li key={l.rel} className="truncate">
                    <span className="font-mono text-ink">{l.rel}</span> → {l.to.slice(0, 3).join(", ")}
                    {l.to.length > 3 ? "…" : ""}
                  </li>
                ))}
              </ul>
            </div>
          ) : null}
        </aside>

        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <FilterBar filters={filters} label={(p) => allCols.find((c) => c.name === p)?.label ?? p} columns={allCols} onChange={writeFilters} />
          <div className="min-h-0 flex-1 overflow-auto">
            {data.isLoading ? (
              <div className="flex flex-col gap-2 p-5">
                {Array.from({ length: 8 }, (_, i) => (
                  <Skeleton key={i} className="h-9 w-full" />
                ))}
              </div>
            ) : data.error ? (
              <div className="p-5">
                <ErrorState error={data.error} retry={() => data.refetch()} />
              </div>
            ) : r!.objects.length === 0 ? (
              <EmptyState
                title={filters.length || q ? "Nothing matches" : `No ${label.toLowerCase()} objects yet`}
                action={
                  filters.length ? (
                    <Button size="sm" onClick={() => writeFilters([])}>
                      Clear filters
                    </Button>
                  ) : summary?.backing?.sync ? (
                    <a className="text-[13px] text-accent underline-offset-2 hover:underline" href={href(["build"])}>
                      Crawl {summary.backing.source} to sync its rows
                    </a>
                  ) : undefined
                }
              >
                {filters.length || q ? "Loosen a filter or search for fewer words." : summary?.backing ? `They are read from ${summary.backing.table} when its source is crawled.` : "They appear here once ingested or created."}
              </EmptyState>
            ) : (
              <table className="w-full min-w-[560px] border-collapse text-[13px]">
                <thead className="sticky top-0 z-[1] bg-page text-left text-xs text-ink-3">
                  <tr className="shadow-[0_1px_0_var(--hairline)]">
                    {visible.map((c, i) => (
                      <SortHeader key={c.name} label={c.label} col={c.name} sort={sort} onSort={sortBy} numeric={c.ty === "int" || c.ty === "float"} first={i === 0} />
                    ))}
                    <SortHeader label="Updated" col="_updated_at" sort={sort} onSort={sortBy} numeric />
                    {rowActions.length ? <th className="w-12 px-3 py-2 font-medium"><span className="sr-only">Actions</span></th> : null}
                  </tr>
                </thead>
                <tbody>
                  {r!.objects.map((e) => (
                    <tr
                      key={e.id}
                      tabIndex={0}
                      onClick={() => navigate(["entities", e.id])}
                      onKeyDown={(ev) => ev.key === "Enter" && ev.target === ev.currentTarget && navigate(["entities", e.id])}
                      className="h-11 cursor-pointer border-b border-hairline transition-shadow duration-150 hover:bg-raised hover:shadow-card focus-visible:bg-raised"
                    >
                      {visible.map((c, i) => (
                        <td key={c.name} className={cx("max-w-[280px] truncate", i === 0 ? "pl-5 pr-3 font-medium text-ink" : "px-3 text-ink-2", (c.ty === "int" || c.ty === "float") && "tnum text-right")}>
                          <Cell entity={e} col={c.name} ty={c.ty} meta={metaOf(c.name)} />
                        </td>
                      ))}
                      <td className="tnum px-5 text-right whitespace-nowrap text-ink-3">{ago(e.updated_at)}</td>
                      {rowActions.length ? (
                        <td className="px-2 text-right">
                          <IconButton
                            label={`Actions on ${e.title || e.id}`}
                            onClick={(ev) => {
                              ev.stopPropagation();
                              setActing({ actions: rowActions, item: e });
                            }}
                            onKeyDown={(ev) => ev.stopPropagation()}
                          >
                            <Zap className="size-4" />
                          </IconButton>
                        </td>
                      ) : null}
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
          {pages > 1 ? (
            <div className="flex items-center justify-end gap-2 border-t border-hairline px-5 py-2 text-[13px] text-ink-2">
              <span className="tnum">
                Page {page + 1} of {pages}
              </span>
              <IconButton label="Previous page" disabled={page === 0} onClick={() => setQuery({ page: page > 1 ? String(page - 1) : undefined })}>
                <ChevronLeft className="size-4" />
              </IconButton>
              <IconButton label="Next page" disabled={page + 1 >= pages} onClick={() => setQuery({ page: String(page + 1) })}>
                <ChevronRight className="size-4" />
              </IconButton>
              {data.isFetching ? <Spinner label="" /> : null}
            </div>
          ) : null}
        </div>
      </div>
      {acting ? <ActionDialog actions={acting.actions} item={acting.item} kind={kind} workspace={acting.item?.workspace ?? workspace} onClose={() => setActing(null)} /> : null}
    </div>
  );
}

function Cell({ entity, col, ty, meta }: { entity: Entity; col: string; ty: PropType; meta?: "title" | "state" }) {
  if (meta === "title") return <>{entity.title || <span className="text-ink-3">{entity.id}</span>}</>;
  if (meta === "state") return <StateBadge entity={entity} />;
  const v = entity.props?.[col];
  if (v === undefined || v === null || v === "") return <span className="text-ink-3">—</span>;
  if (ty === "timestamp" && typeof v === "number") return <time dateTime={new Date(v).toISOString()}>{when(v)}</time>;
  if (ty === "bool") return <>{v ? "yes" : "no"}</>;
  if (typeof v === "number") return <>{v.toLocaleString()}</>;
  return <>{display(v)}</>;
}

function SortHeader({ label, col, sort, onSort, numeric, first }: { label: string; col: string; sort: string; onSort: (c: string) => void; numeric?: boolean; first?: boolean }) {
  const dir = sort === col ? "ascending" : sort === `-${col}` ? "descending" : "none";
  return (
    <th aria-sort={dir} className={cx("py-1 font-medium", first ? "pl-4 pr-2" : "px-2", numeric && "text-right")}>
      <button onClick={() => onSort(col)} className={cx("inline-flex h-7 items-center gap-1 rounded-input px-1 hover:bg-sunken hover:text-ink", dir !== "none" && "text-ink")}>
        {label}
        {dir === "ascending" ? <ArrowUp className="size-3" aria-hidden /> : dir === "descending" ? <ArrowDown className="size-3" aria-hidden /> : null}
      </button>
    </th>
  );
}

function FacetGroup({ prop, label, values, filters, onToggle }: { prop: string; label: string; values: { value: Json; count: number }[]; filters: Filter[]; onToggle: (v: Json) => void }) {
  const current = filters.find((f) => f.prop === prop && (f.op === "eq" || f.op === "in"));
  const picked = new Set(current ? (current.op === "in" && Array.isArray(current.value) ? current.value : [current.value ?? null]).map((v) => JSON.stringify(v)) : []);
  // Picked values stay listed even when no object has them any more.
  const shown = [...values];
  for (const p of picked) if (!shown.some((v) => JSON.stringify(v.value) === p)) shown.push({ value: JSON.parse(p), count: 0 });
  const max = Math.max(1, ...shown.map((v) => v.count));
  return (
    <div className="mt-5">
      <h3 className="mb-1.5 text-[13px] font-semibold">{label}</h3>
      <ul className="flex flex-wrap gap-1.5" aria-label={`${label} values`}>
        {shown.map((v) => {
          const on = picked.has(JSON.stringify(v.value));
          return (
            <li key={JSON.stringify(v.value)}>
              <button
                aria-pressed={on}
                onClick={() => onToggle(v.value)}
                className={cx(
                  "relative inline-flex h-7 items-center gap-1.5 overflow-hidden rounded-full px-2.5 text-xs transition-colors duration-150",
                  on ? "bg-accent text-on-accent" : "bg-raised text-ink shadow-card hover:bg-sunken",
                )}
              >
                {!on ? <span aria-hidden className="absolute inset-y-0 left-0 bg-accent-tint" style={{ width: `${(v.count / max) * 100}%` }} /> : null}
                <span className="relative max-w-[150px] truncate">{v.value === null ? "none" : display(v.value)}</span>
                <span className={cx("tnum relative", on ? "opacity-80" : "text-ink-3")}>{v.count.toLocaleString()}</span>
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

const nf = new Intl.NumberFormat(undefined, { maximumFractionDigits: 2 });

function StatsGroup({ prop, label, stats, timestamp, onRange }: { prop: string; label: string; stats: Stats; timestamp: boolean; onRange: (lo: number, hi: number) => void }) {
  const fmt = (n: number) => (timestamp ? new Date(n).toLocaleDateString() : nf.format(n));
  const hist = stats.histogram ?? [];
  const max = Math.max(1, ...hist.map((b) => b.count));
  return (
    <div className="mt-5">
      <h3 className="mb-1 text-[13px] font-semibold">{label}</h3>
      {hist.length ? (
        <div className="flex h-12 items-end gap-px" role="group" aria-label={`${label} distribution: pick a bar to filter`}>
          {hist.map((b, i) => (
            <button
              key={i}
              title={`${fmt(b.lo)} – ${fmt(b.hi)}: ${b.count}`}
              aria-label={`${prop} ${fmt(b.lo)} to ${fmt(b.hi)}, ${b.count} objects`}
              disabled={b.count === 0}
              onClick={() => onRange(b.lo, b.hi)}
              className="group flex h-full flex-1 items-end rounded-[2px] focus-visible:outline-offset-0"
            >
              <span className="block w-full rounded-[2px] bg-accent/70 transition-colors duration-150 group-hover:bg-accent group-disabled:bg-hairline" style={{ height: `${Math.max(4, (b.count / max) * 100)}%` }} />
            </button>
          ))}
        </div>
      ) : null}
      <dl className="mt-1.5 grid grid-cols-2 gap-x-3 gap-y-0.5 text-xs">
        <dt className="text-ink-3">min</dt>
        <dd className="tnum text-right text-ink">{fmt(stats.min)}</dd>
        <dt className="text-ink-3">max</dt>
        <dd className="tnum text-right text-ink">{fmt(stats.max)}</dd>
        {stats.mean !== undefined ? (
          <>
            <dt className="text-ink-3">mean</dt>
            <dd className="tnum text-right text-ink">{fmt(stats.mean)}</dd>
          </>
        ) : null}
        {stats.sum !== undefined ? (
          <>
            <dt className="text-ink-3">sum</dt>
            <dd className="tnum text-right text-ink">{nf.format(stats.sum)}</dd>
          </>
        ) : null}
        <dt className="text-ink-3">count</dt>
        <dd className="tnum text-right text-ink">{stats.count.toLocaleString()}</dd>
      </dl>
    </div>
  );
}

function FilterBar({ filters, label, columns, onChange }: { filters: Filter[]; label: (p: string) => string; columns: ExploreResult["columns"]; onChange: (f: Filter[]) => void }) {
  const [adding, setAdding] = useState(false);
  return (
    <div className="flex flex-wrap items-center gap-1.5 border-b border-hairline px-4 py-2 md:px-5">
      {filters.length === 0 && !adding ? <span className="text-xs text-ink-3">No filters</span> : null}
      {filters.map((f, i) => (
        <span key={i} className="inline-flex h-7 items-center gap-1 rounded-full bg-accent-tint pr-1 pl-2.5 text-xs text-ink">
          <span className="font-medium">{label(f.prop)}</span>
          <span className="text-ink-2">{OP_LABEL[f.op]}</span>
          {f.op !== "exists" && f.op !== "missing" ? <span className="max-w-[180px] truncate font-mono">{Array.isArray(f.value) ? f.value.map(display).join(", ") : display(f.value)}</span> : null}
          <button aria-label={`Remove filter on ${label(f.prop)}`} className="rounded-full p-1 text-ink-3 hover:bg-raised hover:text-ink" onClick={() => onChange(filters.filter((_, j) => j !== i))}>
            <X className="size-3" />
          </button>
        </span>
      ))}
      {adding ? (
        <AddFilter
          columns={columns}
          onAdd={(f) => {
            onChange([...filters, f]);
            setAdding(false);
          }}
          onCancel={() => setAdding(false)}
        />
      ) : (
        <Button size="sm" variant="ghost" className="h-7" icon={<Plus className="size-3.5" />} onClick={() => setAdding(true)}>
          Filter
        </Button>
      )}
      {filters.length ? (
        <Button size="sm" variant="ghost" className="ml-auto h-7" onClick={() => onChange([])}>
          Clear all
        </Button>
      ) : null}
    </div>
  );
}

function AddFilter({ columns, onAdd, onCancel }: { columns: ExploreResult["columns"]; onAdd: (f: Filter) => void; onCancel: () => void }) {
  const [prop, setProp] = useState(columns[0]?.name ?? "title");
  const [op, setOp] = useState<FilterOp>("eq");
  const [value, setValue] = useState("");
  const ty = columns.find((c) => c.name === prop)?.ty;
  const numeric = ty === "int" || ty === "float";
  const ops: FilterOp[] = numeric || ty === "timestamp" ? ["eq", "ne", "gt", "gte", "lt", "lte", "exists", "missing"] : ["eq", "ne", "contains", "exists", "missing"];
  // The condition shown and the one sent: a condition the new property does not take falls back to "is".
  const effOp: FilterOp = ops.includes(op) ? op : "eq";
  const needsValue = effOp !== "exists" && effOp !== "missing";
  const submit = () => {
    if (needsValue && !value.trim()) return;
    const v: Json = !needsValue ? null : numeric && Number.isFinite(Number(value)) ? Number(value) : ty === "bool" ? value.trim() === "true" : value.trim();
    onAdd({ prop, op: effOp, value: v });
  };
  return (
    <form
      className="flex flex-wrap items-center gap-1.5"
      onSubmit={(e) => {
        e.preventDefault();
        submit();
      }}
      onKeyDown={(e) => e.key === "Escape" && onCancel()}
    >
      <Select
        autoFocus
        aria-label="Property"
        value={prop}
        onChange={(e) => {
          setProp(e.target.value);
          setOp("eq");
        }}
        className="h-7 w-32 text-xs"
      >
        {columns.map((c) => (
          <option key={c.name} value={c.name}>
            {c.label}
          </option>
        ))}
      </Select>
      <Select aria-label="Condition" value={effOp} onChange={(e) => setOp(e.target.value as FilterOp)} className="h-7 w-24 text-xs">
        {ops.map((o) => (
          <option key={o} value={o}>
            {OP_LABEL[o]}
          </option>
        ))}
      </Select>
      {needsValue ? <Input aria-label="Value" value={value} onChange={(e) => setValue(e.target.value)} placeholder={ty === "timestamp" ? "2026-10-17" : "value"} className="h-7 w-32 text-xs" /> : null}
      <Button size="sm" type="submit" className="h-7">
        Add
      </Button>
      <Button size="sm" type="button" variant="ghost" className="h-7" onClick={onCancel}>
        Cancel
      </Button>
    </form>
  );
}

function ColumnChooser({ columns, visible, onChange }: { columns: ExploreResult["columns"]; visible: string[]; onChange: (cols: string[]) => void }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => ref.current && !ref.current.contains(e.target as Node) && setOpen(false);
    window.addEventListener("mousedown", onDown);
    return () => window.removeEventListener("mousedown", onDown);
  }, [open]);
  return (
    <div ref={ref} className="relative" onKeyDown={(e) => e.key === "Escape" && setOpen(false)}>
      <Button size="sm" variant="ghost" icon={<Columns3 className="size-4" />} aria-expanded={open} aria-haspopup="true" onClick={() => setOpen((o) => !o)}>
        <span className="hidden sm:inline">Columns</span>
      </Button>
      {open ? (
        <div className="absolute right-0 z-20 mt-1 flex max-h-[60dvh] w-56 flex-col gap-1.5 overflow-y-auto rounded-card bg-raised p-3 shadow-overlay">
          {columns.map((c) => (
            <Checkbox
              key={c.name}
              label={c.label}
              checked={visible.includes(c.name)}
              onChange={(on) => onChange(on ? columns.map((x) => x.name).filter((n) => n === c.name || visible.includes(n)) : visible.filter((n) => n !== c.name))}
            />
          ))}
          {columns.length === 0 ? <p className="text-xs text-ink-3">No columns</p> : null}
        </div>
      ) : null}
    </div>
  );
}

/** Small badge for a property type (used by the builder too). */
export function TypeBadge({ ty }: { ty: string }) {
  return (
    <Badge tone={ty === "enum" ? "accent" : "neutral"} className="font-mono">
      {ty}
    </Badge>
  );
}
