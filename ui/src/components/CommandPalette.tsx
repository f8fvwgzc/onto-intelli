import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Blocks, Boxes, GitPullRequestArrow, Map as MapIcon, Search, Shapes, Table2 } from "lucide-react";

import { call } from "../lib/api";
import { kindColor, kindLabel } from "../lib/format";
import { useSchema, useScope } from "../lib/queries";
import { navigate } from "../lib/router";
import type { Entity } from "../lib/types";
import { Dot, Kbd, cx } from "./ui";

interface Item {
  key: string;
  label: string;
  hint?: string;
  icon: React.ReactNode;
  run: () => void;
}

/** A view the palette lists besides the ontology's (the console's pages). */
export interface PaletteView {
  key: string;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
}

/** ⌘K: jump to any entity (on the map, or its page with ⇧) or any view (`views` first: the console's). */
export function CommandPalette({ open, onClose, views: extra = [] }: { open: boolean; onClose: () => void; views?: readonly PaletteView[] }) {
  const [q, setQ] = useState("");
  const [debounced, setDebounced] = useState("");
  const [index, setIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const schema = useSchema().data;
  const scope = useScope();

  useEffect(() => {
    const t = window.setTimeout(() => setDebounced(q.trim()), 140);
    return () => window.clearTimeout(t);
  }, [q]);

  useEffect(() => {
    const d = dialogRef.current;
    if (!d) return;
    if (open && !d.open) {
      d.showModal();
      setQ("");
      setIndex(0);
      requestAnimationFrame(() => inputRef.current?.focus());
    }
    if (!open && d.open) d.close();
  }, [open]);

  const results = useQuery<{ hits: { entity: Entity; score: number }[] }>({
    queryKey: ["palette", debounced, ...scope],
    queryFn: () => call("entity.search", { query: debounced, limit: 8 }),
    enabled: open && debounced.length > 1,
    placeholderData: (prev) => prev,
  });

  const items: Item[] = useMemo(() => {
    const go = (path: string[], query: Record<string, string> = {}) => () => {
      navigate(path, query);
      onClose();
    };
    const views: Item[] = [
      ...extra.map((v) => ({ key: `v:${v.key}`, label: v.label, hint: "View", icon: <v.icon className="size-4" />, run: go([v.key]) })),
      { key: "v:map", label: "Map", hint: "View", icon: <MapIcon className="size-4" />, run: go(["map"]) },
      { key: "v:explore", label: "Explorer", hint: "View", icon: <Table2 className="size-4" />, run: go(["explore"]) },
      { key: "v:entities", label: "Entities", hint: "View", icon: <Boxes className="size-4" />, run: go(["entities"]) },
      { key: "v:schema", label: "Schema", hint: "View", icon: <Shapes className="size-4" />, run: go(["schema"]) },
      { key: "v:build", label: "Builder", hint: "View", icon: <Blocks className="size-4" />, run: go(["build"]) },
      { key: "v:proposals", label: "Proposals", hint: "View", icon: <GitPullRequestArrow className="size-4" />, run: go(["proposals"]) },
    ].filter((v) => !q || v.label.toLowerCase().includes(q.toLowerCase()));
    const types: Item[] = q
      ? Object.keys(schema?.object_types ?? {})
          .filter((k) => k.includes(q.toLowerCase().replace(/\s+/g, "_")))
          .slice(0, 4)
          .map((k) => ({
            key: `t:${k}`,
            label: kindLabel(k, schema),
            hint: "Object type",
            icon: <Dot color={kindColor(k, schema)} />,
            run: go(["schema", "object_types", k]),
          }))
          .flatMap((t) => [t, { ...t, key: `x:${t.key.slice(2)}`, hint: "Explore objects", run: go(["explore", t.key.slice(2)]) }])
      : [];
    const hits: Item[] = (results.data?.hits ?? []).map(({ entity }) => ({
      key: `e:${entity.id}`,
      label: entity.title || entity.id,
      hint: `${kindLabel(entity.kind, schema)} · ${entity.id}`,
      icon: <Dot color={kindColor(entity.kind, schema)} />,
      run: go(["map"], { focus: entity.id }),
    }));
    return [...hits, ...types, ...views];
  }, [q, results.data, schema, onClose, extra]);

  useEffect(() => setIndex(0), [items.length]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setIndex((i) => Math.min(items.length - 1, i + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setIndex((i) => Math.max(0, i - 1));
    } else if (e.key === "Enter") {
      const item = items[index];
      if (!item) return;
      e.preventDefault();
      if (e.shiftKey && item.key.startsWith("e:")) {
        navigate(["entities", item.key.slice(2)]);
        onClose();
      } else item.run();
    }
  };

  return (
    <dialog
      ref={dialogRef}
      onClose={onClose}
      onClick={(e) => e.target === dialogRef.current && onClose()}
      aria-label="Search and jump"
      className="mx-auto mt-[12dvh] w-[calc(100vw-24px)] max-w-xl overflow-hidden rounded-modal bg-raised p-0 text-ink shadow-overlay backdrop:bg-black/25"
    >
      <div className="flex items-center gap-2.5 border-b border-hairline px-4">
        <Search className="size-4 text-ink-3" aria-hidden />
        <input
          ref={inputRef}
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={onKeyDown}
          role="combobox"
          aria-expanded
          aria-controls="palette-list"
          aria-activedescendant={items[index] ? `palette-${index}` : undefined}
          placeholder="Search entities, types and views"
          className="h-12 flex-1 bg-transparent text-[15px] outline-none placeholder:text-ink-3"
        />
        <Kbd>esc</Kbd>
      </div>
      <ul id="palette-list" role="listbox" className="max-h-[52dvh] overflow-y-auto p-1.5">
        {items.length === 0 ? (
          <li className="px-3 py-6 text-center text-sm text-ink-3">{results.isFetching ? "Searching…" : "Nothing matches"}</li>
        ) : (
          items.map((it, i) => (
            <li
              key={it.key}
              id={`palette-${i}`}
              role="option"
              aria-selected={i === index}
              onMouseMove={() => setIndex(i)}
              onClick={it.run}
              className={cx("flex cursor-pointer items-center gap-3 rounded-input px-3 py-2", i === index && "bg-sunken")}
            >
              <span className="flex w-4 justify-center text-ink-2">{it.icon}</span>
              <span className="min-w-0 flex-1 truncate text-sm text-ink">{it.label}</span>
              {it.hint ? <span className="truncate font-mono text-xs text-ink-3">{it.hint}</span> : null}
            </li>
          ))
        )}
      </ul>
      <div className="flex items-center gap-3 border-t border-hairline px-4 py-2 text-xs text-ink-3">
        <span>
          <Kbd>↵</Kbd> show on map
        </span>
        <span>
          <Kbd>⇧</Kbd> <Kbd>↵</Kbd> open entity
        </span>
      </div>
    </dialog>
  );
}
