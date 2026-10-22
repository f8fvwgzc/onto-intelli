import { useQuery } from "@tanstack/react-query";
import { CircleCheck, Circle } from "lucide-react";

import { BarList, LineChart, Sparkline } from "../../components/charts";
import { Card, Page, Stat, Status } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { ErrorState, Segmented, Skeleton } from "../../components/ui";
import { consoleApi, qk, useConsoleScope } from "../../lib/console";
import { ago, delta, ms, num, pct, usd } from "../../lib/format";
import { useScope } from "../../lib/queries";
import { DAY, fillSeries, observe, type Metrics } from "../../lib/observe";
import { href, setQuery, useRoute } from "../../lib/router";
import { minuteNow } from "./ProjectsPage";

const WINDOWS = {
  "24h": { ms: DAY, bucket: "hour" as const, label: "24 hours" },
  "7d": { ms: 7 * DAY, bucket: "day" as const, label: "7 days" },
  "30d": { ms: 30 * DAY, bucket: "day" as const, label: "30 days" },
};
type Win = keyof typeof WINDOWS;

function Checklist({ hasKeys, hasLlm }: { hasKeys: boolean; hasLlm: boolean }) {
  const steps = [
    { done: hasKeys, title: "Create an API key", body: "A key reaches this project's data plane: REST, gRPC and MCP.", to: "keys" },
    { done: false, title: "Send the first request", body: "Copy a curl, TypeScript, Python or MCP snippet from Connect; its trace shows up here.", to: "connect" },
    { done: hasLlm, title: "Bring your own LLM (optional)", body: "Use your Anthropic, OpenAI or compatible account, with a budget.", to: "llm" },
    { done: false, title: "Invite your team (optional)", body: "Members sign in with your identity provider.", to: "members" },
  ];
  return (
    <Card title="Get connected" aside={<span className="text-xs text-ink-3">Shown until the first trace</span>}>
      <ol className="flex flex-col gap-1">
        {steps.map((s) => (
          <li key={s.title}>
            <a href={href([s.to])} className="flex gap-3 rounded-input p-2 hover:bg-sunken">
              {s.done ? (
                <CircleCheck className="mt-0.5 size-4 shrink-0 text-success" aria-label="Done" />
              ) : (
                <Circle className="mt-0.5 size-4 shrink-0 text-ink-3" aria-label="To do" />
              )}
              <span className="min-w-0">
                <span className={s.done ? "block text-[13px] font-medium text-ink-3 line-through" : "block text-[13px] font-medium text-ink"}>{s.title}</span>
                <span className="block text-xs text-ink-3">{s.body}</span>
              </span>
            </a>
          </li>
        ))}
      </ol>
    </Card>
  );
}

function Cards({ m, prev, label, series }: { m: Metrics; prev?: Metrics; label: string; series: number[] }) {
  const d = (cur: number, p: number | undefined, lower?: boolean) =>
    prev === undefined || p === undefined ? { text: `last ${label}`, tone: "flat" as const } : delta(cur, p, lower);
  const req = d(m.requests, prev?.requests);
  const err = d(m.error_rate, prev?.error_rate, true);
  const p95 = d(m.p95_ms, prev?.p95_ms, true);
  const tok = d(m.input_tokens + m.output_tokens, prev ? prev.input_tokens + prev.output_tokens : undefined);
  const cost = d(m.cost_usd, prev?.cost_usd, true);
  const det = d(m.deterministic_share, prev?.deterministic_share);
  const cache = d(m.cache_hit_rate, prev?.cache_hit_rate);
  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
      <div className="col-span-2">
        <Stat primary label="Requests" value={num(m.requests)} sub={req.text} tone={req.tone}>
          <Sparkline values={series} label={`Requests per bucket, last ${label}`} />
        </Stat>
      </div>
      <Stat label="Error rate" value={pct(m.error_rate)} sub={`${num(m.errors)} errors · ${err.text}`} tone={err.tone} />
      <Stat label="Latency p50" value={ms(m.p50_ms)} sub={`p95 ${ms(m.p95_ms)} · ${p95.text}`} tone={p95.tone} />
      <Stat label="Tokens" value={num(m.input_tokens + m.output_tokens)} sub={tok.text} />
      <Stat label="Cost" value={usd(m.cost_usd)} sub={cost.text} tone={cost.tone} />
      <Stat label="Deterministic answers" value={pct(m.deterministic_share)} sub={det.text} />
      <Stat label="Cache hit rate" value={pct(m.cache_hit_rate)} sub={cache.text} />
    </div>
  );
}

export default function OverviewPage() {
  const route = useRoute();
  const scope = useScope();
  const { project } = useConsoleScope();
  const win: Win = (route.query.get("range") as Win) in WINDOWS ? (route.query.get("range") as Win) : "7d";
  const w = WINDOWS[win];
  const until = minuteNow() + 60_000;
  const since = until - w.ms;
  const cur = useQuery({ queryKey: ["metrics", ...scope, win, until], queryFn: () => observe.metrics({ since, until, bucket: w.bucket }), placeholderData: (p) => p });
  const prev = useQuery({
    queryKey: ["metrics", ...scope, win, "prev", until],
    queryFn: () => observe.metrics({ since: since - w.ms, until: since, bucket: w.bucket }),
    placeholderData: (p) => p,
  });
  const errors = useQuery({ queryKey: ["traces", ...scope, "errors"], queryFn: () => observe.traces({ status: "error", limit: 5 }), refetchInterval: 30_000 });
  const any = useQuery({ queryKey: ["traces", ...scope, "any"], queryFn: () => observe.traces({ limit: 1 }) });
  const keys = useQuery({ queryKey: qk.keys(project?.ref ?? ""), queryFn: () => consoleApi.keys(project!.ref), enabled: Boolean(project) });
  const llm = useQuery({ queryKey: qk.llm(project?.ref ?? ""), queryFn: () => consoleApi.llm(project!.ref), enabled: Boolean(project) });
  const noTraces = any.data ? any.data.traces.length === 0 : false;
  const series = cur.data ? fillSeries(cur.data.series, since, until, w.bucket) : [];
  const labels = series.map((s) => s.bucket);

  return (
    <>
      <PageHeader title="Overview" context={project ? `${project.name} · ${project.ref}` : undefined}>
        <Segmented
          label="Time range"
          value={win}
          onChange={(v) => setQuery({ range: v === "7d" ? undefined : v })}
          options={[
            { value: "24h", label: "24h" },
            { value: "7d", label: "7d" },
            { value: "30d", label: "30d" },
          ]}
        />
      </PageHeader>
      <Page>
        <div className="flex flex-col gap-3">
          {cur.error ? <ErrorState error={cur.error} retry={() => void cur.refetch()} /> : null}
          {noTraces ? <Checklist hasKeys={Boolean(keys.data?.keys.some((k) => !k.revoked_at))} hasLlm={Boolean(llm.data?.llm)} /> : null}
          {cur.data ? (
            <Cards m={cur.data} prev={prev.data} label={w.label} series={series.map((s) => s.requests)} />
          ) : !cur.error ? (
            <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
              <Skeleton className="col-span-2 h-[132px] rounded-card" />
              {[0, 1, 2, 3, 4, 5].map((i) => (
                <Skeleton key={i} className="h-[92px] rounded-card" />
              ))}
            </div>
          ) : null}
          <div className="grid gap-3 lg:grid-cols-3">
            <Card title="Requests and errors" className="lg:col-span-2">
              {cur.data ? (
                <LineChart
                  label={`Requests and errors per ${w.bucket}, last ${w.label}`}
                  labels={labels}
                  series={[
                    { name: "Requests", values: series.map((s) => s.requests), color: "var(--chart-1)" },
                    { name: "Errors", values: series.map((s) => s.errors), color: "var(--danger)" },
                  ]}
                />
              ) : (
                <Skeleton className="h-[204px]" />
              )}
            </Card>
            <Card title="Cost by model">
              {cur.data ? (
                <BarList
                  rows={cur.data.by_model.map((b) => ({ label: b.model || "unknown", value: b.cost_usd, hint: `${num(b.calls)} calls` }))}
                  format={usd}
                  empty="No LLM calls in this range"
                />
              ) : (
                <Skeleton className="h-[160px]" />
              )}
            </Card>
            <Card title="Latency" className="lg:col-span-2">
              {cur.data ? (
                <LineChart
                  label={`Latency p50 and p95 per ${w.bucket}`}
                  labels={labels}
                  format={ms}
                  area={false}
                  series={[
                    { name: "p50", values: series.map((s) => s.p50_ms), color: "var(--chart-1)" },
                    { name: "p95", values: series.map((s) => s.p95_ms), color: "var(--chart-2)", dashed: true },
                  ]}
                />
              ) : (
                <Skeleton className="h-[204px]" />
              )}
            </Card>
            <Card
              title="Recent errors"
              aside={
                <a className="text-xs text-accent hover:underline" href={href(["traces"], { status: "error" })}>
                  All errors
                </a>
              }
            >
              {errors.data?.traces.length ? (
                <ul className="-mx-2 flex flex-col">
                  {errors.data.traces.map((t) => (
                    <li key={t.id}>
                      <a href={href(["traces", t.id])} className="block rounded-input px-2 py-1.5 hover:bg-sunken">
                        <span className="flex items-center gap-2">
                          <Status tone="error">{t.name}</Status>
                          <span className="ml-auto shrink-0 text-xs text-ink-3">{ago(t.ts)}</span>
                        </span>
                        <span className="block truncate pl-3 text-xs text-ink-3">
                          {t.error_code ?? "error"}
                          {t.error_message ? `: ${t.error_message}` : ""}
                        </span>
                      </a>
                    </li>
                  ))}
                </ul>
              ) : errors.isLoading ? (
                <Skeleton className="h-24" />
              ) : (
                <p className="py-6 text-center text-[13px] text-ink-3">No errors recorded</p>
              )}
            </Card>
          </div>
          {cur.data && cur.data.scores.length ? (
            <Card title="Scores">
              <ul className="flex flex-wrap gap-x-6 gap-y-2">
                {cur.data.scores.map((s) => (
                  <li key={s.name} className="text-[13px]">
                    <span className="font-mono text-ink">{s.name}</span>{" "}
                    <span className="tnum text-ink-2">
                      {s.avg === null ? "labels" : s.avg.toFixed(2)} avg · {num(s.count)}
                    </span>
                  </li>
                ))}
              </ul>
            </Card>
          ) : null}
        </div>
      </Page>
    </>
  );
}
