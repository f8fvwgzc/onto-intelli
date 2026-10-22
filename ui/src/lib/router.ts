// Hash routes, so the server only ever serves one page and nexc can link to
// /ui/#/map?workspace=main&focus=project:alpha.

import { useSyncExternalStore } from "react";

export interface Route {
  path: string[];
  query: URLSearchParams;
}

function parse(): Route {
  const raw = window.location.hash.replace(/^#\/?/, "");
  const [p, q = ""] = raw.split("?");
  const path = p.split("/").filter(Boolean).map(decodeURIComponent);
  return { path: path.length ? path : ["map"], query: new URLSearchParams(q) };
}

let route = parse();
const listeners = new Set<() => void>();
window.addEventListener("hashchange", () => {
  route = parse();
  listeners.forEach((l) => l());
});

export function useRoute(): Route {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => route,
  );
}

export function href(path: string[], query: Record<string, string | undefined> = {}): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) if (v) q.set(k, v);
  const qs = q.toString();
  return `#/${path.map(encodeURIComponent).join("/")}${qs ? `?${qs}` : ""}`;
}

export function navigate(path: string[], query: Record<string, string | undefined> = {}) {
  window.location.hash = href(path, query);
}

/** Change query parameters of the current route without adding history. */
export function setQuery(patch: Record<string, string | undefined>) {
  const q = new URLSearchParams(route.query);
  for (const [k, v] of Object.entries(patch)) {
    if (v === undefined || v === "") q.delete(k);
    else q.set(k, v);
  }
  const qs = q.toString();
  const next = `#/${route.path.map(encodeURIComponent).join("/")}${qs ? `?${qs}` : ""}`;
  window.history.replaceState(null, "", next);
  route = parse();
  listeners.forEach((l) => l());
}

/** Go to a route in place of the current history entry, updating listeners at once. */
export function replaceRoute(path: string[], query: Record<string, string | undefined> = {}) {
  window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}${href(path, query)}`);
  route = parse();
  listeners.forEach((l) => l());
}
