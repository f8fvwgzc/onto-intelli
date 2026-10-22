import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { ArrowLeft } from "lucide-react";

import { Card, Page, Status, td, th } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { Button, EmptyState, ErrorState, Select, Skeleton, cx } from "../../components/ui";
import { ago, ms, num, usd, when } from "../../lib/format";
import { useScope } from "../../lib/queries";
import { RANGES, observe, rangeMs } from "../../lib/observe";
import { href, navigate, setQuery, useRoute } from "../../lib/router";
import { minuteNow } from "./ProjectsPage";

function Replay({ id }: { id: string }) {
  const scope = useScope();
  const q = useQuery({ queryKey: ["session", ...scope, id], queryFn: () => observe.session(id) });
  const traces = q.data?.traces ?? [];
  const totals = traces.reduce((a, t) => ({ ms: a.ms + t.duration_ms, tok: a.tok + t.input_tokens + t.output_tokens, cost: a.cost + t.cost_usd }), { ms: 0, tok: 0, cost: 0 });
  return (
    <>
      <PageHeader title="Session" context={<span className="font-mono">{id}</span>}>
        <a href={href(["sessions"])} className="inline-flex h-8 items-center gap-1.5 rounded-input px-2.5 text-[13px] font-medium text-ink-2 hover:bg-sunken hover:text-ink">
          <ArrowLeft className="size-4" aria-hidden /> Sessions
        </a>
      </PageHeader>
      <Page>
        {q.error ? (
          <ErrorState error={q.error} retry={() => void q.refetch()} />
        ) : q.isLoading ? (
          <Skeleton className="h-48 rounded-card" />
        ) : (
          <div className="mx-auto flex max-w-3xl flex-col gap-3">
            <p className="text-[13px] text-ink-2">
              {num(traces.length)} turns · {ms(totals.ms)} in total · {num(totals.tok)} tokens · {usd(totals.cost)}
              {traces[0]?.user_id ? ` · ${traces[0].user_id}` : ""}
            </p>
            <ol className="flex flex-col gap-3">
              {traces.map((t, i) => (
                <li key={t.id}>
                  <Card>
                    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-ink-3">
                      <span className="tnum font-medium text-ink-2">Turn {i + 1}</span>
                      <span>{when(t.ts)}</span>
                      <span className="tnum">{ms(t.duration_ms)}</span>
                      <span>{t.mode ?? t.kind}</span>
                      {t.status === "error" ? <Status tone="error">{t.error_code ?? "error"}</Status> : null}
                      <a href={href(["traces", t.id])} className="ml-auto text-accent hover:underline">
                        Trace
                      </a>
                    </div>
                    <p className="mt-2 text-[13px] font-medium text-ink">{t.input?.question ?? <span className="text-ink-3">{t.name} (no stored question)</span>}</p>
                    {t.output?.answer ? <p className="mt-1.5 text-[13px] whitespace-pre-wrap text-ink-2">{t.output.answer}</p> : null}
                  </Card>
                </li>
              ))}
            </ol>
          </div>
        )}
      </Page>
    </>
  );
}

export default function SessionsPage({ id }: { id?: string }) {
  const route = useRoute();
  const scope = useScope();
  const range = route.query.get("range") ?? "7d";
  const since = range === "all" ? undefined : minuteNow() - rangeMs(range);
  const q = useInfiniteQuery({
    queryKey: ["sessions", ...scope, range],
    queryFn: ({ pageParam }) => observe.sessions({ since, cursor: pageParam || undefined, limit: 50 }),
    initialPageParam: "",
    getNextPageParam: (last) => last.next ?? undefined,
    enabled: !id,
  });
  if (id) return <Replay id={id} />;
  const list = q.data?.pages.flatMap((p) => p.sessions) ?? [];

  return (
    <>
      <PageHeader title="Sessions" context="Traces grouped by session id (x-onto-session-id, or the conversation)">
        <Select
          aria-label="Time range"
          value={range}
          onChange={(e) => setQuery({ range: e.target.value === "7d" ? undefined : e.target.value })}
          className="h-8 max-w-44 text-[13px]"
        >
          {RANGES.map((r) => (
            <option key={r.value} value={r.value}>
              {r.label}
            </option>
          ))}
          <option value="all">All time</option>
        </Select>
      </PageHeader>
      <Page>
        {q.error ? (
          <ErrorState error={q.error} retry={() => void q.refetch()} />
        ) : q.isLoading ? (
          <Skeleton className="h-48 rounded-card" />
        ) : !list.length ? (
          <EmptyState title="No sessions in this range">
            Send <code className="font-mono text-[13px]">x-onto-session-id</code> with requests, or continue a conversation, to group its turns here.
          </EmptyState>
        ) : (
          <>
            {/* The fixed columns take about 690 px; the question keeps at least 200 px and the table scrolls sideways when narrower. */}
            <div className="overflow-x-auto overscroll-x-contain">
            <table className="stack-table w-full table-fixed border-collapse text-[13px] md:min-w-[900px]">
              <thead>
                <tr className="border-b border-hairline">
                  <th className={cx(th, "w-[180px]")}>Session</th>
                  <th className={th}>First question</th>
                  <th className={cx(th, "w-[120px]")}>User</th>
                  <th className={cx(th, "w-[64px] text-right")}>Turns</th>
                  <th className={cx(th, "w-[64px] text-right")}>Errors</th>
                  <th className={cx(th, "w-[76px] text-right")}>Tokens</th>
                  <th className={cx(th, "w-[76px] text-right")}>Cost</th>
                  <th className={cx(th, "w-[112px]")}>Last active</th>
                </tr>
              </thead>
              <tbody>
                {list.map((s) => (
                  <tr
                    key={s.session_id}
                    tabIndex={0}
                    onClick={() => navigate(["sessions", s.session_id])}
                    onKeyDown={(e) => e.key === "Enter" && navigate(["sessions", s.session_id])}
                    className="cursor-pointer border-b border-hairline outline-none hover:bg-sunken focus-visible:bg-accent-tint"
                  >
                    <td className={td} data-primary>
                      <span className="block truncate font-mono text-[12.5px] text-ink">{s.session_id}</span>
                    </td>
                    <td className={cx(td, "text-ink-2")} data-label="First question">
                      <span className="block truncate">{s.preview ?? "–"}</span>
                    </td>
                    <td className={cx(td, "truncate text-ink-2")} data-label="User">
                      {s.user_id ?? "–"}
                    </td>
                    <td className={cx(td, "tnum text-right")} data-label="Turns">
                      {num(s.traces)}
                    </td>
                    <td className={cx(td, "tnum text-right", s.errors ? "text-danger" : "text-ink-3")} data-label="Errors">
                      {num(s.errors)}
                    </td>
                    <td className={cx(td, "tnum text-right text-ink-2")} data-label="Tokens">
                      {num(s.input_tokens + s.output_tokens)}
                    </td>
                    <td className={cx(td, "tnum text-right text-ink-2")} data-label="Cost">
                      {usd(s.cost_usd)}
                    </td>
                    <td className={cx(td, "whitespace-nowrap text-ink-2")} data-label="Last active" title={when(s.last_ts)}>
                      {ago(s.last_ts)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            </div>
            {q.hasNextPage ? (
              <div className="mt-4 flex justify-center">
                <Button size="sm" busy={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>
                  Load more
                </Button>
              </div>
            ) : null}
          </>
        )}
      </Page>
    </>
  );
}
