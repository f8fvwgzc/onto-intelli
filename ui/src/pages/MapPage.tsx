import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Crosshair, History, Maximize2, Minus, Plus, Route as RouteIcon, Search, SlidersHorizontal, Waypoints, Zap } from "lucide-react";

import { EntityDetails, EntityTitle, isAlert, useEntity } from "../components/EntityCard";
import { GraphCanvas, pairKey, type GraphCanvasHandle } from "../components/GraphCanvas";
import { PageHeader } from "../components/Shell";
import { Button, Dot, EmptyState, ErrorState, IconButton, Panel, Segmented, Spinner, cx } from "../components/ui";
import { call, errorText } from "../lib/api";
import { kindColor, kindLabel, plural, when } from "../lib/format";
import { useSchema, useScope, useWhoami } from "../lib/queries";
import { href, setQuery, useRoute } from "../lib/router";
import { useSession } from "../lib/session";
import type { DepReport, Edge, Entity, Hop, Overview, RootCause, Subgraph } from "../lib/types";

type Mode = "explore" | "path" | "impact";

const edgeKey = (e: Edge) => `${e.from}\u0000${e.rel}\u0000${e.to}`;

function stub(n: { id: string; kind: string; title: string; state?: string }): Entity {
  return { id: n.id, kind: n.kind, title: n.title, state: n.state, workspace: "", created_at: 0, updated_at: 0, revision: 0 };
}

export function MapPage() {
  const route = useRoute();
  const session = useSession();
  const scope = useScope();
  const me = useWhoami().data;
  const schema = useSchema().data;
  const workspace = session.workspace || me?.default_workspace;
  const canvas = useRef<GraphCanvasHandle>(null);

  const [nodes, setNodes] = useState(new Map<string, Entity>());
  const [edges, setEdges] = useState(new Map<string, Edge>());
  const nodesRef = useRef(nodes);
  nodesRef.current = nodes;
  const [selected, setSelected] = useState<string | undefined>(route.query.get("focus") ?? undefined);
  const [mode, setMode] = useState<Mode>((route.query.get("mode") as Mode) || "explore");
  const [picked, setPicked] = useState<string[]>([]);
  const [highlight, setHighlight] = useState<{ nodes: Set<string>; edges: Set<string>; tone: "accent" | "danger"; note?: string } | null>(null);
  const [hidden, setHidden] = useState(new Set<string>());
  const [filtersOpen, setFiltersOpen] = useState(false);
  const [history, setHistory] = useState<{ on: boolean; ts: number }>({ on: false, ts: Date.now() });
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const overview = useQuery<Overview>({
    queryKey: ["overview", workspace, ...scope],
    queryFn: () => call<Overview>("graph.overview", { workspace, limit: 160 }),
    enabled: !!me,
  });

  const merge = useCallback((g: Partial<Subgraph>, replace = false) => {
    setNodes((prev) => {
      const next = replace ? new Map<string, Entity>() : new Map(prev);
      for (const n of g.nodes ?? []) {
        const old = next.get(n.id);
        // Keep the full document when a lighter stub arrives later.
        next.set(n.id, old && n.revision === 0 ? old : n);
      }
      return next;
    });
    setEdges((prev) => {
      const next = replace ? new Map<string, Edge>() : new Map(prev);
      for (const e of g.edges ?? []) next.set(edgeKey(e), e);
      return next;
    });
  }, []);

  useEffect(() => {
    if (overview.data) merge(overview.data, true);
  }, [overview.data, merge]);

  const expand = useCallback(
    async (id: string, focus = false) => {
      setBusy("Expanding");
      try {
        const sub = await call<Subgraph>("graph.traverse", { start: [id], depth: 1, limit: 80 });
        merge(sub);
        if (sub.truncated) setNotice("Showing the first 80 neighbours");
        if (focus) requestAnimationFrame(() => setTimeout(() => canvas.current?.center(id), 350));
      } catch (e) {
        setNotice(errorText(e));
      } finally {
        setBusy(null);
      }
    },
    [merge],
  );

  // A link (or ⌘K) can name an entity to focus on.
  const focusParam = route.query.get("focus");
  useEffect(() => {
    if (!focusParam || !me) return;
    setSelected(focusParam);
    void expand(focusParam, true);
  }, [focusParam, me, expand]);

  const select = useCallback(
    (id: string | undefined) => {
      if (mode === "path" && id) {
        setPicked((p) => (p.length >= 2 ? [id] : p.includes(id) ? p : [...p, id]));
        setSelected(id);
        return;
      }
      setSelected(id);
      setQuery({ focus: id });
    },
    [mode],
  );

  // Path mode: two picks, then the shortest connection.
  useEffect(() => {
    if (mode !== "path" || picked.length !== 2) return;
    let cancelled = false;
    (async () => {
      setBusy("Finding a path");
      try {
        const r = await call<{ found: boolean; hops: Hop[]; text?: string }>("graph.path", { from: picked[0], to: picked[1], depth: 6 });
        if (cancelled) return;
        if (!r.found) {
          setHighlight(null);
          setNotice("These two are not connected within 6 steps");
          return;
        }
        const ids = new Set<string>([picked[0], ...r.hops.map((h) => h.to)]);
        const missing = [...ids].filter((id) => !nodesRef.current.has(id));
        const fetched = await Promise.all(missing.map((id) => call<Entity>("entity.get", { id }).catch(() => null)));
        merge({ nodes: fetched.filter(Boolean) as Entity[], edges: r.hops.map((h) => ({ from: h.from, rel: h.rel, to: h.to })) });
        setHighlight({ nodes: ids, edges: new Set(r.hops.map((h) => pairKey(h.from, h.to))), tone: "accent", note: r.text });
      } catch (e) {
        setNotice(errorText(e));
      } finally {
        if (!cancelled) setBusy(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [mode, picked, merge]);

  // Impact mode: what fails with the selection, and why it is failing.
  useEffect(() => {
    if (mode !== "impact" || !selected) return;
    let cancelled = false;
    (async () => {
      setBusy("Tracing impact");
      try {
        const [impact, cause] = await Promise.all([
          call<DepReport>("graph.impact", { id: selected, depth: 4 }),
          call<RootCause>("graph.root_cause", { id: selected, depth: 6 }),
        ]);
        if (cancelled) return;
        const chain = cause.chains[0];
        const hops = [...impact.edges, ...(chain?.hops ?? [])];
        merge({
          nodes: [...impact.nodes.map(stub), ...(chain ? [stub(chain.culprit)] : [])],
          edges: hops.map((h) => ({ from: h.from, rel: h.rel, to: h.to })),
        });
        const ids = new Set<string>([selected, ...impact.nodes.map((n) => n.id)]);
        if (chain) {
          setHighlight({
            nodes: new Set([selected, ...chain.hops.map((h) => h.to), chain.culprit.id]),
            edges: new Set(chain.hops.map((h) => pairKey(h.from, h.to))),
            tone: "danger",
            note: `Likely cause: ${chain.text}.${impact.nodes.length ? ` ${plural(impact.nodes.length, "thing")} ${impact.nodes.length === 1 ? "depends" : "depend"} on it.` : ""}`,
          });
        } else {
          setHighlight({
            nodes: ids,
            edges: new Set(impact.edges.map((h) => pairKey(h.from, h.to))),
            tone: "accent",
            note: impact.nodes.length ? `${plural(impact.nodes.length, "thing")} affected if this fails` : "Nothing depends on this",
          });
        }
      } catch (e) {
        setNotice(errorText(e));
      } finally {
        if (!cancelled) setBusy(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [mode, selected, merge]);

  useEffect(() => {
    if (mode === "explore") setHighlight(null);
    if (mode !== "path") setPicked([]);
    setQuery({ mode: mode === "explore" ? undefined : mode });
  }, [mode]);

  // History: the graph around the selection (or the busiest entities) at a time.
  const visibleNodes = useMemo(() => [...nodes.values()], [nodes]);
  const earliest = useMemo(() => {
    const ts = visibleNodes.map((n) => n.created_at).filter((t) => t > 0);
    return ts.length ? Math.min(...ts) : Date.now() - 30 * 864e5;
  }, [visibleNodes]);
  const [debouncedTs, setDebouncedTs] = useState(history.ts);
  useEffect(() => {
    const t = window.setTimeout(() => setDebouncedTs(history.ts), 220);
    return () => window.clearTimeout(t);
  }, [history.ts]);
  const seeds = useMemo(
    () => (selected ? [selected] : (overview.data?.nodes ?? []).slice(0, 6).map((n) => n.id)),
    [selected, overview.data],
  );
  const snapshot = useQuery<Subgraph>({
    queryKey: ["snapshot", seeds.join(","), debouncedTs, ...scope],
    queryFn: () => call<Subgraph>("graph.snapshot", { start: seeds, ts: debouncedTs, depth: 2, limit: 200 }),
    enabled: history.on && seeds.length > 0,
    placeholderData: (prev) => prev,
  });

  const shown = useMemo(() => {
    const src = history.on && snapshot.data ? snapshot.data : { nodes: visibleNodes, edges: [...edges.values()] };
    const ns = src.nodes.filter((n) => !hidden.has(n.kind));
    const ids = new Set(ns.map((n) => n.id));
    return { nodes: ns, edges: src.edges.filter((e) => ids.has(e.from) && ids.has(e.to)) };
  }, [history.on, snapshot.data, visibleNodes, edges, hidden]);

  const kinds = useMemo(() => {
    const counts = new Map<string, number>();
    for (const n of nodes.values()) counts.set(n.kind, (counts.get(n.kind) ?? 0) + 1);
    return [...counts.entries()].sort((a, b) => b[1] - a[1]);
  }, [nodes]);

  const colorOf = useCallback((k: string) => kindColor(k, schema), [schema]);
  const alertOf = useCallback((e: Entity) => isAlert(e, schema), [schema]);
  const selectedEntity = useEntity(selected);

  const modeHint =
    mode === "path"
      ? picked.length === 0
        ? "Pick the first entity"
        : picked.length === 1
          ? "Pick the second entity"
          : highlight?.note
      : mode === "impact"
        ? selected
          ? highlight?.note
          : "Pick an entity to see what depends on it"
        : undefined;

  useEffect(() => {
    if (!notice) return;
    const t = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(t);
  }, [notice]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Map"
        context={
          overview.data
            ? `${workspace ?? "all workspaces"} · showing ${shown.nodes.length.toLocaleString()} of ${overview.data.total.toLocaleString()} entities`
            : workspace
        }
      >
        <Segmented<Mode>
          label="Map mode"
          value={mode}
          onChange={setMode}
          options={[
            { value: "explore", label: "Explore", icon: <Waypoints className="size-3.5" /> },
            { value: "path", label: "Path", icon: <RouteIcon className="size-3.5" /> },
            { value: "impact", label: "Impact", icon: <Zap className="size-3.5" /> },
          ]}
        />
        <Button size="sm" variant={filtersOpen || hidden.size ? "secondary" : "ghost"} icon={<SlidersHorizontal className="size-4" />} onClick={() => setFiltersOpen((o) => !o)} aria-expanded={filtersOpen}>
          Kinds{hidden.size ? ` · ${kinds.length - hidden.size}/${kinds.length}` : ""}
        </Button>
        <Button size="sm" variant={history.on ? "secondary" : "ghost"} icon={<History className="size-4" />} aria-pressed={history.on} onClick={() => setHistory((h) => ({ on: !h.on, ts: Date.now() }))}>
          History
        </Button>
      </PageHeader>

      {filtersOpen ? (
        <div className="flex flex-wrap items-center gap-1.5 border-b border-hairline px-4 py-2 md:px-5" role="group" aria-label="Kinds shown">
          {kinds.map(([k, n]) => {
            const off = hidden.has(k);
            return (
              <button
                key={k}
                aria-pressed={!off}
                onClick={() =>
                  setHidden((h) => {
                    const next = new Set(h);
                    if (off) next.delete(k);
                    else next.add(k);
                    return next;
                  })
                }
                className={cx(
                  "inline-flex h-7 items-center gap-1.5 rounded-full px-2.5 text-[12px] font-medium transition-colors duration-150",
                  off ? "text-ink-3 line-through decoration-ink-3/60" : "bg-raised text-ink shadow-card",
                )}
              >
                <Dot color={colorOf(k)} className={off ? "opacity-40" : ""} />
                {kindLabel(k, schema)}
                <span className="tnum text-ink-3">{n}</span>
              </button>
            );
          })}
          {hidden.size ? (
            <button onClick={() => setHidden(new Set())} className="ml-1 text-[12px] text-ink-2 underline-offset-2 hover:underline">
              Show all
            </button>
          ) : null}
        </div>
      ) : null}

      <div className="relative flex min-h-0 flex-1">
        <div className="canvas-grid relative min-h-0 min-w-0 flex-1">
          {overview.isLoading || !me ? (
            <div className="grid size-full place-items-center">
              <Spinner label="Laying out the map" />
            </div>
          ) : overview.error ? (
            <div className="p-5">
              <ErrorState error={overview.error} retry={() => overview.refetch()} />
            </div>
          ) : shown.nodes.length === 0 && !history.on ? (
            <EmptyState title="This workspace has no entities yet">
              Ingest data or create entities through the API, then come back. <a className="text-accent underline-offset-2 hover:underline" href={href(["schema"])}>Review the schema</a> while you wait.
            </EmptyState>
          ) : (
            <GraphCanvas
              ref={canvas}
              ariaLabel="Workspace map"
              nodes={shown.nodes}
              edges={shown.edges}
              colorOf={colorOf}
              isAlert={alertOf}
              selected={selected}
              picked={picked}
              highlightNodes={highlight?.nodes}
              highlightEdges={highlight?.edges}
              highlightTone={highlight?.tone}
              onSelect={select}
              onExpand={(id) => void expand(id)}
            />
          )}

          <div className="absolute top-3 left-3 flex flex-col gap-1 rounded-card bg-raised p-1 shadow-card">
            <IconButton label="Zoom in" onClick={() => canvas.current?.zoomBy(1.25)}>
              <Plus className="size-4" />
            </IconButton>
            <IconButton label="Zoom out" onClick={() => canvas.current?.zoomBy(0.8)}>
              <Minus className="size-4" />
            </IconButton>
            <IconButton label="Fit to view" onClick={() => canvas.current?.fit()}>
              <Maximize2 className="size-4" />
            </IconButton>
            {selected ? (
              <IconButton label="Center on selection" onClick={() => canvas.current?.center(selected)}>
                <Crosshair className="size-4" />
              </IconButton>
            ) : null}
          </div>

          {modeHint || busy || notice ? (
            <div aria-live="polite" className="pointer-events-none absolute top-3 right-3 left-16 flex justify-end">
              <p className="pointer-events-auto max-w-md rounded-card bg-raised px-3 py-2 text-[13px] text-ink-2 shadow-card">
                {busy ? <Spinner label={busy} /> : notice ?? modeHint}
              </p>
            </div>
          ) : null}

          {history.on ? (
            <div className="absolute inset-x-3 bottom-3 rounded-card bg-raised px-4 py-3 shadow-overlay md:inset-x-auto md:left-1/2 md:w-[min(640px,calc(100%-24px))] md:-translate-x-1/2">
              <div className="flex items-center gap-3">
                <label htmlFor="history-slider" className="text-[13px] font-medium whitespace-nowrap">
                  As of
                </label>
                <span className="tnum min-w-0 flex-1 truncate text-[13px] text-ink-2">
                  {when(history.ts)}
                  {snapshot.isFetching ? " · loading" : snapshot.data ? ` · ${plural(snapshot.data.nodes.length, "entity", "entities")}` : ""}
                </span>
                <Button size="sm" variant="ghost" onClick={() => setHistory({ on: true, ts: Date.now() })}>
                  Now
                </Button>
              </div>
              <input
                id="history-slider"
                type="range"
                min={earliest}
                max={Date.now()}
                step={Math.max(60_000, Math.round((Date.now() - earliest) / 400))}
                value={history.ts}
                onChange={(e) => setHistory({ on: true, ts: Number(e.target.value) })}
                className="mt-2 w-full accent-[var(--accent)]"
                aria-valuetext={when(history.ts)}
              />
              <p className="mt-1 text-xs text-ink-3">
                {selected ? "Around the selection, two steps out." : "Around the busiest entities. Select one to follow it through time."}
              </p>
            </div>
          ) : null}
        </div>

        <Panel
          open={!!selected && !(mode === "path" && picked.length === 2)}
          onClose={() => select(undefined)}
          title={selectedEntity.data ? <EntityTitle entity={selectedEntity.data} schema={schema} /> : <span className="font-mono text-[13px]">{selected}</span>}
        >
          {selected ? (
            <EntityDetails
              id={selected}
              schema={schema}
              onPick={(id) => {
                select(id);
                if (!nodes.has(id)) void expand(id, true);
                else canvas.current?.center(id);
              }}
              footer={
                <div className="mt-6 flex flex-wrap gap-2">
                  <Button size="sm" icon={<Waypoints className="size-4" />} onClick={() => void expand(selected)}>
                    Expand
                  </Button>
                  <Button size="sm" icon={<Zap className="size-4" />} onClick={() => setMode("impact")}>
                    Impact
                  </Button>
                  <a className="inline-flex h-8 items-center gap-1.5 rounded-input px-2.5 text-[13px] font-medium text-ink-2 hover:bg-sunken hover:text-ink" href={href(["entities", selected])}>
                    <Search className="size-4" aria-hidden /> History and details
                  </a>
                </div>
              }
            />
          ) : null}
        </Panel>
      </div>
    </div>
  );
}
