import { useQuery } from "@tanstack/react-query";

import { Page, td, th } from "../../components/consoleUi";
import { Checkbox, Dot, EmptyState, ErrorState, Skeleton } from "../../components/ui";
import { call } from "../../lib/api";
import { display, plural } from "../../lib/format";
import { useScope } from "../../lib/queries";
import { setQuery, useRoute } from "../../lib/router";
import type { ShapeReport } from "../../lib/types";
import { Means, NotDone, SEVERITY_DOT, Verdict } from "./common";

const LIMIT = 500;

export function ShapesTab() {
  const route = useRoute();
  const scope = useScope();
  const inferred = route.query.get("inferred") === "1";
  const q = useQuery<ShapeReport>({
    queryKey: ["shapes", "validate", inferred, ...scope],
    queryFn: () => call<ShapeReport>("shapes.validate", { inferred, limit: LIMIT }),
    placeholderData: (prev) => prev,
  });
  const r = q.data;
  const skipped = (r?.skipped ?? []).filter((s) => s.key !== "deactivated");

  return (
    <Page>
      <div className="flex flex-wrap items-center gap-3">
        <Checkbox label="Read inferred facts (derived classes and relations)" checked={inferred} onChange={(v) => setQuery({ inferred: v ? "1" : undefined })} />
        {q.isFetching && r ? <span className="text-xs text-ink-3">Updating</span> : null}
      </div>
      {q.error ? (
        <div className="mt-4">
          <ErrorState error={q.error} retry={() => void q.refetch()} />
        </div>
      ) : null}
      {q.isLoading ? <Skeleton className="mt-4 h-20" /> : null}
      {r ? (
        <div className="mt-4 flex flex-col gap-4">
          {r.verdict === "conforms" ? (
            <Verdict tone="ok" word="conforms">
              Every evaluated constraint holds on {plural(r.focus_nodes, "focus node")}.
            </Verdict>
          ) : r.verdict === "violates" ? (
            <Verdict tone="bad" word="violates">
              {plural(r.counts.violation, "violation")}, {plural(r.counts.warning, "warning")}, {plural(r.counts.info, "note")} across {plural(r.focus_nodes, "focus node")}.
            </Verdict>
          ) : (
            <Verdict tone="unsure" word="undetermined">
              {r.fixpoint_reached === false ? "The closure stopped before its fixpoint" : "Some constraints were not evaluated"}, so silence here is not a pass.
              {r.total_results ? ` ${plural(r.total_results, "result")} found so far.` : ""}
            </Verdict>
          )}

          {skipped.length ? (
            <NotDone title="Not evaluated" items={skipped.map((s) => `${s.shape}${s.constraint_id ? ` ${s.constraint_id}` : ""} · ${s.key}`)}>
              {skipped.length === 1 ? skipped[0].reason : "Each of these was skipped, so the report cannot say they hold."}
            </NotDone>
          ) : null}
          {r.unmatched_targets?.length ? (
            <p className="text-[13px] text-ink-2">
              Targets that selected nothing: <span className="font-mono">{r.unmatched_targets.join(", ")}</span>
            </p>
          ) : null}

          {r.results.length ? (
            <div className="min-w-0 overflow-x-auto rounded-card bg-raised shadow-card">
              <table className="stack-table w-full text-[13px]">
                <thead>
                  <tr className="border-b border-hairline">
                    <th className={th}>Severity</th>
                    <th className={th}>Shape</th>
                    <th className={th}>Component</th>
                    <th className={th}>Focus</th>
                    <th className={th}>Path</th>
                    <th className={th}>Message</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-hairline">
                  {r.results.map((x, i) => (
                    <tr key={`${x.constraint_id}/${x.focus}/${i}`}>
                      <td className={td} data-label="Severity">
                        <span className="inline-flex items-center gap-1.5 text-ink-2">
                          <Dot color={SEVERITY_DOT[x.severity]} />
                          {x.severity}
                        </span>
                      </td>
                      <td className={`${td} font-mono text-[12.5px]`} data-label="Shape" title={x.constraint_id}>
                        {x.shape}
                      </td>
                      <td className={`${td} font-mono text-[12.5px] text-ink-2`} data-label="Component">
                        {x.component}
                      </td>
                      <td className={`${td} font-mono text-[12.5px] md:whitespace-nowrap`} data-label="Focus">
                        {x.focus}
                      </td>
                      <td className={`${td} font-mono text-[12.5px] text-ink-2`} data-label="Path">
                        {x.path ?? "–"}
                      </td>
                      <td className={`${td} text-ink`} data-label="Message">
                        {x.message}
                        {x.value !== undefined ? <span className="block font-mono text-xs text-ink-3">value {display(x.value)}</span> : null}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : r.verdict === "conforms" ? null : (
            <EmptyState title="No results to list" />
          )}
          {r.truncated ? <p className="tnum text-xs text-ink-3">Showing {r.results.length} of {r.total_results} results.</p> : null}
          <Means>{r.means}</Means>
        </div>
      ) : null}
    </Page>
  );
}
