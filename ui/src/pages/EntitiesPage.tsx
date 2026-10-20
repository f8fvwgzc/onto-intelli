import { useEffect, useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { ChevronLeft, ChevronRight, Map as MapIcon } from "lucide-react";

import { EntityDetails, EntityTitle, StateBadge, useEntity } from "../components/EntityCard";
import { PageHeader } from "../components/Shell";
import { Button, Dot, EmptyState, ErrorState, IconButton, Input, Select, Skeleton, Spinner, cx } from "../components/ui";
import { call } from "../lib/api";
import { ago, kindColor, kindLabel, when } from "../lib/format";
import { useSchema, useScope, useWhoami } from "../lib/queries";
import { href, navigate, setQuery, useRoute } from "../lib/router";
import { useSession } from "../lib/session";
import type { Entity, HistoryEntry } from "../lib/types";

const PAGE = 50;

export function EntitiesPage() {
  const route = useRoute();
  const id = route.path[1];
  return id ? <EntityPage id={id} /> : <EntityList />;
}

function EntityList() {
  const route = useRoute();
  const schema = useSchema().data;
  const scope = useScope();
  const me = useWhoami().data;
  const session = useSession();
  const workspace = session.workspace || me?.default_workspace;
  const kind = route.query.get("kind") ?? "";
  const q = route.query.get("q") ?? "";
  const page = Number(route.query.get("page") ?? 0);
  const [text, setText] = useState(q);
  useEffect(() => {
    const t = window.setTimeout(() => text !== q && setQuery({ q: text || undefined, page: undefined }), 250);
    return () => window.clearTimeout(t);
  }, [text, q]);

  const data = useQuery<{ rows: Entity[]; total: number }>({
    queryKey: ["entities", kind, q, page, workspace, ...scope],
    queryFn: async () => {
      if (q.trim()) {
        const r = await call<{ hits: { entity: Entity }[] }>("entity.search", { query: q, kinds: kind || undefined, workspace, limit: PAGE });
        return { rows: r.hits.map((h) => h.entity), total: r.hits.length };
      }
      const r = await call<{ entities: Entity[]; total: number }>("entity.find", { kind: kind || undefined, workspace, limit: PAGE, offset: page * PAGE });
      return { rows: r.entities, total: r.total };
    },
    placeholderData: keepPreviousData,
    enabled: !!me,
  });

  const kinds = Object.keys(schema?.object_types ?? {}).sort();
  const total = data.data?.total ?? 0;
  const pages = Math.max(1, Math.ceil(total / PAGE));

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title="Entities" context={data.data ? `${total.toLocaleString()} ${kind ? kindLabel(kind, schema).toLowerCase() : "entities"}${q ? ` matching “${q}”` : ""}` : workspace}>
        <Input aria-label="Search entities" placeholder="Search" value={text} onChange={(e) => setText(e.target.value)} className="h-8 w-44 sm:w-60" />
        <Select aria-label="Kind" value={kind} onChange={(e) => setQuery({ kind: e.target.value || undefined, page: undefined })} className="h-8 w-40">
          <option value="">All kinds</option>
          {kinds.map((k) => (
            <option key={k} value={k}>
              {kindLabel(k, schema)}
            </option>
          ))}
        </Select>
      </PageHeader>
      <div className="min-h-0 flex-1 overflow-auto">
        {data.isLoading ? (
          <div className="flex flex-col gap-2 p-5">
            {Array.from({ length: 8 }, (_, i) => (
              <Skeleton key={i} className="h-9 w-full" />
            ))}
          </div>
        ) : data.error ? (
          <div className="p-5"><ErrorState error={data.error} retry={() => data.refetch()} /></div>
        ) : data.data!.rows.length === 0 ? (
          <EmptyState title={q ? "Nothing matches" : "No entities of this kind"}>{q ? "Try fewer words or another kind." : "They appear here once ingested."}</EmptyState>
        ) : (
          <table className="w-full min-w-[640px] border-collapse text-[13px]">
            <thead className="sticky top-0 z-[1] bg-page text-left text-xs text-ink-3">
              <tr className="shadow-[0_1px_0_var(--hairline)]">
                <th className="px-5 py-2 font-medium">Title</th>
                <th className="px-3 py-2 font-medium">Kind</th>
                <th className="px-3 py-2 font-medium">State</th>
                <th className="px-3 py-2 font-medium">Id</th>
                <th className="px-5 py-2 text-right font-medium">Updated</th>
              </tr>
            </thead>
            <tbody>
              {data.data!.rows.map((e) => (
                <tr
                  key={e.id}
                  tabIndex={0}
                  onClick={() => navigate(["entities", e.id])}
                  onKeyDown={(ev) => ev.key === "Enter" && navigate(["entities", e.id])}
                  className="h-11 cursor-pointer border-b border-hairline transition-shadow duration-150 hover:bg-raised hover:shadow-card focus-visible:bg-raised"
                >
                  <td className="max-w-[360px] truncate px-5 font-medium text-ink">{e.title || <span className="text-ink-3">untitled</span>}</td>
                  <td className="px-3 whitespace-nowrap text-ink-2">
                    <span className="inline-flex items-center gap-1.5">
                      <Dot color={kindColor(e.kind, schema)} />
                      {kindLabel(e.kind, schema)}
                    </span>
                  </td>
                  <td className="px-3">
                    <StateBadge entity={e} schema={schema} />
                  </td>
                  <td className="max-w-[220px] truncate px-3 font-mono text-xs text-ink-3">{e.id}</td>
                  <td className="tnum px-5 text-right whitespace-nowrap text-ink-3">{ago(e.updated_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      {!q && pages > 1 ? (
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
  );
}

function EntityPage({ id }: { id: string }) {
  const schema = useSchema().data;
  const scope = useScope();
  const entity = useEntity(id);
  const [asOf, setAsOf] = useState<number | null>(null);
  const history = useQuery<{ history: HistoryEntry[] }>({
    queryKey: ["history", id, ...scope],
    queryFn: () => call("entity.history", { id, limit: 100 }),
  });
  const past = useQuery<{ entity: Entity | null }>({
    queryKey: ["as_of", id, asOf, ...scope],
    queryFn: () => call("entity.as_of", { id, ts: asOf }),
    enabled: asOf !== null,
  });

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title={entity.data?.title || id} context={entity.data ? `${kindLabel(entity.data.kind, schema)} · ${id}` : id}>
        <a href={href(["map"], { focus: id })} className="inline-flex h-8 items-center gap-1.5 rounded-input px-2.5 text-[13px] font-medium text-ink-2 hover:bg-sunken hover:text-ink">
          <MapIcon className="size-4" aria-hidden /> Show on map
        </a>
      </PageHeader>
      <div className="grid min-h-0 flex-1 overflow-y-auto lg:grid-cols-[minmax(0,1fr)_380px] lg:overflow-hidden">
        <div className="px-4 py-4 md:px-6 lg:overflow-y-auto">
          <a href={href(["entities"])} className="text-[13px] text-ink-2">
            ← Entities
          </a>
          {entity.data ? <div className="mt-3"><EntityTitle entity={entity.data} schema={schema} /></div> : null}
          <EntityDetails id={id} schema={schema} onPick={(other) => navigate(["entities", other])} />
        </div>
        <aside className="border-hairline px-4 py-4 md:px-6 lg:overflow-y-auto lg:border-l">
          <h3 className="text-[13px] font-semibold">History</h3>
          <p className="text-xs text-ink-3">Every committed version, newest first. Pick one to see the entity as it was.</p>
          {history.isLoading ? (
            <div className="mt-4"><Spinner /></div>
          ) : history.error ? (
            <div className="mt-4"><ErrorState error={history.error} /></div>
          ) : (
            <ol className="relative mt-4 flex flex-col gap-0.5 before:absolute before:top-2 before:bottom-2 before:left-[5px] before:w-px before:bg-hairline">
              {history.data!.history.map((h) => (
                <li key={`${h.revision}-${h.ts}`}>
                  <button
                    onClick={() => setAsOf(asOf === h.ts ? null : h.ts)}
                    aria-pressed={asOf === h.ts}
                    className={cx("relative flex w-full gap-3 rounded-input py-1.5 pr-2 pl-0 text-left", asOf === h.ts ? "bg-sunken" : "hover:bg-sunken")}
                  >
                    <span className={cx("relative z-[1] mt-1.5 size-[11px] shrink-0 rounded-full border-2 border-page", h.deleted ? "bg-danger" : asOf === h.ts ? "bg-accent" : "bg-[var(--gray-3)]")} aria-hidden />
                    <span className="min-w-0">
                      <span className="block text-[13px] text-ink">
                        {h.deleted ? "Deleted" : h.changed.join(", ") || "Saved"}
                        {h.state ? <span className="text-ink-3"> · {h.state}</span> : null}
                      </span>
                      <span className="block text-xs text-ink-3">
                        {h.actor} · <time dateTime={h.at}>{when(h.ts)}</time> · rev <span className="tnum">{h.revision}</span>
                      </span>
                    </span>
                  </button>
                </li>
              ))}
            </ol>
          )}
          {asOf !== null ? (
            <div className="mt-5 rounded-card bg-raised p-3 shadow-card">
              <p className="text-[13px] font-medium">As of {when(asOf)}</p>
              {past.isLoading ? (
                <Spinner />
              ) : past.data?.entity ? (
                <pre className="mt-2 max-h-80 overflow-auto font-mono text-[11.5px] leading-relaxed text-ink-2">{JSON.stringify({ title: past.data.entity.title, state: past.data.entity.state, props: past.data.entity.props }, null, 2)}</pre>
              ) : (
                <p className="mt-1 text-[13px] text-ink-3">It did not exist then.</p>
              )}
              <Button size="sm" variant="ghost" className="mt-2" onClick={() => setAsOf(null)}>
                Back to now
              </Button>
            </div>
          ) : null}
        </aside>
      </div>
    </div>
  );
}
