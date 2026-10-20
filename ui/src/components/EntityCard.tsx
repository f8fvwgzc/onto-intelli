import { useMemo, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { ArrowRight } from "lucide-react";

import { call } from "../lib/api";
import { ago, display, kindColor, kindLabel } from "../lib/format";
import { useScope } from "../lib/queries";
import type { Entity, SchemaDoc } from "../lib/types";
import { Badge, Dot, ErrorState, Section, Skeleton } from "./ui";

export function isAlert(e: Pick<Entity, "kind" | "state">, schema?: SchemaDoc): boolean {
  return !!e.state && !!schema?.object_types?.[e.kind]?.alert_states?.includes(e.state);
}

export function StateBadge({ entity, schema }: { entity: Pick<Entity, "kind" | "state">; schema?: SchemaDoc }) {
  if (!entity.state) return null;
  return <Badge tone={isAlert(entity, schema) ? "danger" : "neutral"}>{entity.state}</Badge>;
}

export function EntityTitle({ entity, schema }: { entity: Entity; schema?: SchemaDoc }) {
  return (
    <div className="min-w-0">
      <p className="flex items-center gap-1.5 text-xs text-ink-2">
        <Dot color={kindColor(entity.kind, schema)} />
        {kindLabel(entity.kind, schema)}
      </p>
      <h2 className="mt-0.5 truncate text-[15px] font-semibold tracking-tight" title={entity.title}>
        {entity.title || entity.id}
      </h2>
    </div>
  );
}

export function useEntity(id: string | undefined) {
  const scope = useScope();
  return useQuery<Entity>({
    queryKey: ["entity", id, ...scope],
    queryFn: () => call<Entity>("entity.get", { id, derived: true }),
    enabled: !!id,
  });
}

interface Neighbor {
  rel: string;
  entity: Entity;
}

export function useNeighbors(id: string | undefined) {
  const scope = useScope();
  return useQuery<{ neighbors: Neighbor[] }>({
    queryKey: ["neighbors", id, ...scope],
    queryFn: () => call("graph.neighbors", { id }),
    enabled: !!id,
  });
}

export function EntityDetails({ id, schema, onPick, footer }: { id: string; schema?: SchemaDoc; onPick?: (id: string) => void; footer?: ReactNode }) {
  const entity = useEntity(id);
  const neighbors = useNeighbors(id);
  const groups = useMemo(() => {
    const m = new Map<string, Entity[]>();
    for (const n of neighbors.data?.neighbors ?? []) m.set(n.rel, [...(m.get(n.rel) ?? []), n.entity]);
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  }, [neighbors.data]);

  if (entity.isLoading) {
    return (
      <div className="mt-4 flex flex-col gap-2">
        <Skeleton className="h-4 w-2/3" />
        <Skeleton className="h-4 w-1/2" />
        <Skeleton className="h-24 w-full" />
      </div>
    );
  }
  if (entity.error) return <div className="mt-4"><ErrorState error={entity.error} retry={() => entity.refetch()} /></div>;
  const e = entity.data!;
  const props = Object.entries(e.props ?? {}).filter(([k]) => k !== "acl");
  const derived = Object.entries(e.derived ?? {});

  return (
    <div>
      <div className="mt-3 flex flex-wrap items-center gap-2 text-xs text-ink-3">
        <StateBadge entity={e} schema={schema} />
        <span className="font-mono">{e.id}</span>
        <span>· updated {ago(e.updated_at)}</span>
        <span className="tnum">· rev {e.revision}</span>
      </div>
      {props.length ? (
        <Section title="Properties">
          <dl className="grid grid-cols-[minmax(84px,auto)_1fr] gap-x-3 gap-y-1.5 text-[13px]">
            {props.map(([k, v]) => (
              <div key={k} className="contents">
                <dt className="truncate text-ink-3">{k}</dt>
                <dd className="min-w-0 break-words text-ink">{display(v)}</dd>
              </div>
            ))}
          </dl>
        </Section>
      ) : null}
      {derived.length ? (
        <Section title="Computed">
          <dl className="grid grid-cols-[minmax(84px,auto)_1fr] gap-x-3 gap-y-1.5 text-[13px]">
            {derived.map(([k, v]) => (
              <div key={k} className="contents">
                <dt className="truncate text-ink-3">{k}</dt>
                <dd className="tnum text-ink">{display(v)}</dd>
              </div>
            ))}
          </dl>
        </Section>
      ) : null}
      <Section title="Relations" aside={<span className="tnum text-xs text-ink-3">{neighbors.data?.neighbors.length ?? 0}</span>}>
        {neighbors.isLoading ? (
          <Skeleton className="h-16 w-full" />
        ) : groups.length === 0 ? (
          <p className="text-[13px] text-ink-3">No relations yet.</p>
        ) : (
          <div className="flex flex-col gap-3">
            {groups.map(([rel, list]) => (
              <div key={rel}>
                <p className="mb-1 font-mono text-xs text-ink-3">{rel}</p>
                <ul className="flex flex-col">
                  {list.slice(0, 12).map((n) => (
                    <li key={n.id}>
                      <button
                        onClick={() => onPick?.(n.id)}
                        className="group flex w-full items-center gap-2 rounded-input px-1.5 py-1 text-left text-[13px] hover:bg-sunken"
                      >
                        <Dot color={kindColor(n.kind, schema)} />
                        <span className="min-w-0 flex-1 truncate">{n.title || n.id}</span>
                        {n.state ? <span className="text-xs text-ink-3">{n.state}</span> : null}
                        <ArrowRight className="size-3.5 text-ink-3 opacity-0 group-hover:opacity-100" aria-hidden />
                      </button>
                    </li>
                  ))}
                  {list.length > 12 ? <li className="px-1.5 text-xs text-ink-3">and {list.length - 12} more</li> : null}
                </ul>
              </div>
            ))}
          </div>
        )}
      </Section>
      {footer}
    </div>
  );
}
