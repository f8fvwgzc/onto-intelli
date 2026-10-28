// The trace list filters the console sends: `q` (a search of the stored
// questions, which the server refuses to everyone but admins) only for admins.

import { test } from "node:test";
import assert from "node:assert/strict";
import { pickFilters, searchesQuestions } from "../src/lib/traceSearch.ts";

test("only project admins and owners search trace questions", () => {
  const roles = ["owner", "admin", "developer", "viewer", undefined] as const;
  assert.deepEqual(roles.map(searchesQuestions), [true, true, false, false, false]);
});

test("a URL with q lists traces for those who may not search, instead of failing", () => {
  const query = new URLSearchParams("q=alpha&status=error&model=");
  const keys = ["status", "mode", "q", "model"];
  assert.deepEqual(pickFilters(query, keys, "admin"), { status: "error", q: "alpha" });
  for (const role of ["developer", "viewer", undefined] as const) {
    assert.deepEqual(pickFilters(query, keys, role), { status: "error" }, String(role));
  }
});
