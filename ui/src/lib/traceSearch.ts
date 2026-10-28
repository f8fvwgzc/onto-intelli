// Which trace list filters the console sends. `q` searches the stored
// questions, which the server allows admins only (a question may quote what
// others cannot read, and matching on it would rebuild it): for everyone
// else the search box is hidden and `q` is never sent, so a link or an old
// URL with `?q=` lists the traces instead of failing.

import type { OrgRole } from "./console";

/** `role` may search trace questions: project admins and owners. */
export function searchesQuestions(role: OrgRole | undefined): boolean {
  return role === "admin" || role === "owner";
}

/** The filters among `keys` that `query` sets, without `q` for those who may not search. */
export function pickFilters(query: URLSearchParams, keys: readonly string[], role: OrgRole | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  for (const k of keys) {
    const v = query.get(k);
    if (v && (k !== "q" || searchesQuestions(role))) out[k] = v;
  }
  return out;
}
