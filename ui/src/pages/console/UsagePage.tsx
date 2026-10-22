import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { TriangleAlert } from "lucide-react";

import { SERIES, StackedBars } from "../../components/charts";
import { Card, Page, Stat, td, th } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { ErrorState, Input, Segmented, Skeleton, cx } from "../../components/ui";
import { consoleApi, qk, useConsoleScope, type UsageRow } from "../../lib/console";
import { num, pct, usd } from "../../lib/format";
import { href, setQuery, useRoute } from "../../lib/router";

type Measure = "cost" | "tokens" | "asks";

function thisMonth(): string {
  return new Date().toISOString().slice(0, 7);
}

/** The first and last day of a month (`YYYY-MM`), the last capped at today. */
function monthRange(m: string): { from: string; to: string; days: string[] } {
  const [y, mo] = m.split("-").map(Number);
  const last = new Date(Date.UTC(y, mo, 0)).getUTCDate();
  const today = new Date().toISOString().slice(0, 10);
  const days: string[] = [];
  for (let d = 1; d <= last; d++) {
    const day = `${m}-${String(d).padStart(2, "0")}`;
    if (day > today) break;
    days.push(day);
  }
  return { from: `${m}-01`, to: days[days.length - 1] ?? `${m}-01`, days };
}

function Budget({ label, used, limit, format }: { label: string; used: number; limit?: number | null; format: (v: number) => string }) {
  if (!limit) {
    return (
      <div className="min-w-0">
        <p className="text-[13px] text-ink-2">{label}</p>
        <p className="tnum mt-1 text-[22px] leading-7 font-semibold tracking-tight">{format(used)}</p>
        <p className="mt-1 text-xs text-ink-3">
          No budget.{" "}
          <a href={href(["llm"])} className="text-accent hover:underline">
            Set one
          </a>
        </p>
      </div>
    );
  }
  const share = used / limit;
  const warn = share >= 0.8;
  return (
    <div className="min-w-0">
      <p className="text-[13px] text-ink-2">{label}</p>
      <p className="tnum mt-1 text-[22px] leading-7 font-semibold tracking-tight">
        {format(used)} <span className="text-[13px] font-normal text-ink-3">of {format(limit)}</span>
      </p>
      <div
        role="progressbar"
        aria-label={`${label}: ${pct(Math.min(1, share))} of the budget`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(Math.min(1, share) * 100)}
        className="mt-2 h-2 overflow-hidden rounded-full bg-sunken"
      >
        <div className={cx("h-2 rounded-full", share >= 1 ? "bg-danger" : warn ? "bg-warning" : "bg-accent")} style={{ width: `${Math.min(100, share * 100)}%` }} />
      </div>
      <p className={cx("mt-1 text-xs", warn ? "text-[color-mix(in_oklch,var(--warning)_75%,var(--text))]" : "text-ink-3")}>
        {share >= 1 ? "Budget reached: LLM steps are skipped until next month." : warn ? `${pct(share)} used: over 80% of the budget.` : `${pct(share)} used`}
      </p>
    </div>
  );
}

export default function UsagePage() {
  const route = useRoute();
  const { project } = useConsoleScope();
  const month = /^\d{4}-\d{2}$/.test(route.query.get("month") ?? "") ? route.query.get("month")! : thisMonth();
  const { from, to, days } = monthRange(month);
  const [picked, setMeasure] = useState<Measure | null>(null);
  const ref = project?.ref ?? "";
  const q = useQuery({ queryKey: qk.usage(ref, from, to), queryFn: () => consoleApi.usage(ref, from, to), enabled: Boolean(project), placeholderData: (p) => p });
  const u = q.data;
  const current = month === thisMonth();

  const byModel = useMemo(() => {
    const m = new Map<string, UsageRow>();
    for (const r of u?.rows ?? []) {
      const key = `${r.provider}/${r.model}`;
      const acc = m.get(key) ?? { ...r, asks: 0, llm_calls: 0, deterministic: 0, input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: 0, errors: 0 };
      for (const k of ["asks", "llm_calls", "deterministic", "input_tokens", "output_tokens", "cached_tokens", "cost_usd", "errors"] as const) acc[k] += r[k];
      m.set(key, acc);
    }
    return [...m.values()].sort((a, b) => b.cost_usd - a.cost_usd || b.llm_calls - a.llm_calls);
  }, [u]);
  const totals = byModel.reduce(
    (a, r) => ({
      asks: a.asks + r.asks,
      calls: a.calls + r.llm_calls,
      det: a.det + r.deterministic,
      tokens: a.tokens + r.input_tokens + r.output_tokens,
      cost: a.cost + r.cost_usd,
    }),
    { asks: 0, calls: 0, det: 0, tokens: 0, cost: 0 },
  );
  // Cost by default; tokens when nothing cost anything (free or unpriced models).
  const measure: Measure = picked ?? (totals.cost > 0 || !totals.tokens ? "cost" : "tokens");
  // The month to date comes from the server's meter for the current month.
  const used = current && u ? { cost: u.month_to_date.cost_usd, tokens: u.month_to_date.tokens } : { cost: totals.cost, tokens: totals.tokens };

  const stacks = useMemo(() => {
    const value = (r: UsageRow) => (measure === "cost" ? r.cost_usd : measure === "tokens" ? r.input_tokens + r.output_tokens : r.asks);
    const top = byModel.slice(0, 5).map((r) => `${r.provider}/${r.model}`);
    const names = [...top, ...(byModel.length > 5 ? ["other"] : [])];
    return names.map((name, i) => ({
      name: name === "other" ? "Other models" : name.replace(/^[^/]*\//, "") || name,
      color: SERIES[i % SERIES.length],
      values: days.map((d) =>
        (u?.rows ?? [])
          .filter((r) => r.day === d && (name === "other" ? !top.includes(`${r.provider}/${r.model}`) : `${r.provider}/${r.model}` === name))
          .reduce((a, r) => a + value(r), 0),
      ),
    }));
  }, [byModel, days, measure, u]);

  return (
    <>
      <PageHeader title="Usage" context="LLM usage and spend, by UTC day">
        <Input
          type="month"
          aria-label="Month"
          value={month}
          max={thisMonth()}
          onChange={(e) => setQuery({ month: e.target.value && e.target.value !== thisMonth() ? e.target.value : undefined })}
          className="h-8 max-w-40 text-[13px]"
        />
      </PageHeader>
      <Page>
        {q.error ? (
          <ErrorState error={q.error} retry={() => void q.refetch()} />
        ) : !u ? (
          <Skeleton className="h-64 rounded-card" />
        ) : (
          <div className="flex flex-col gap-3">
            {u.budget.reached && current ? (
              <p role="alert" className="flex items-center gap-2 rounded-card bg-danger-tint p-3 text-[13px] text-danger">
                <TriangleAlert className="size-4 shrink-0" aria-hidden /> {u.budget.reached}
              </p>
            ) : null}
            <Card>
              <div className="grid gap-5 sm:grid-cols-2">
                <Budget label={current ? "Cost, month to date" : "Cost in the month"} used={used.cost} limit={current ? u.budget.monthly_budget_usd : null} format={usd} />
                <Budget label={current ? "Tokens, month to date" : "Tokens in the month"} used={used.tokens} limit={current ? u.budget.monthly_token_budget : null} format={num} />
              </div>
            </Card>
            <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
              <Stat label="Asks" value={num(totals.asks)} />
              <Stat label="LLM calls" value={num(totals.calls)} />
              <Stat label="Deterministic answers" value={num(totals.det)} sub={totals.asks ? `${pct(totals.det / totals.asks)} of asks` : undefined} />
              <Stat label="Rate limit" value={u.budget.asks_per_minute ? `${num(u.budget.asks_per_minute)}/min` : "None"} />
            </div>
            <Card
              title="By day and model"
              aside={
                <Segmented
                  label="Measure"
                  value={measure}
                  onChange={setMeasure}
                  options={[
                    { value: "cost", label: "Cost" },
                    { value: "tokens", label: "Tokens" },
                    { value: "asks", label: "Asks" },
                  ]}
                />
              }
            >
              <StackedBars label={`Daily ${measure} by model`} labels={days} stacks={stacks} format={measure === "cost" ? usd : num} />
            </Card>
            <Card title="By model" pad={false}>
              {byModel.length ? (
                <div className="px-4 pb-2">
                  <div className="overflow-x-auto overscroll-x-contain">
                    <table className="stack-table w-full border-collapse text-[13px]">
                      <thead>
                        <tr className="border-b border-hairline">
                          <th className={th}>Model</th>
                          <th className={cx(th, "text-right")}>Asks</th>
                          <th className={cx(th, "text-right")}>LLM calls</th>
                          <th className={cx(th, "text-right")}>Deterministic</th>
                          <th className={cx(th, "text-right")}>Input tokens</th>
                          <th className={cx(th, "text-right")}>Output tokens</th>
                          <th className={cx(th, "text-right")}>Errors</th>
                          <th className={cx(th, "text-right")}>Cost</th>
                        </tr>
                      </thead>
                      <tbody>
                        {byModel.map((r) => (
                          <tr key={`${r.provider}/${r.model}`} className="border-b border-hairline last:border-0">
                            <td className={td} data-primary>
                              <span className="font-mono text-[12.5px]">{r.model || "default"}</span> <span className="text-xs text-ink-3">{r.provider}</span>
                            </td>
                            {[r.asks, r.llm_calls, r.deterministic, r.input_tokens, r.output_tokens, r.errors].map((v, i) => (
                              <td
                                key={i}
                                className={cx(td, "tnum text-right text-ink-2")}
                                data-label={["Asks", "LLM calls", "Deterministic", "Input tokens", "Output tokens", "Errors"][i]}
                              >
                                {num(v)}
                              </td>
                            ))}
                            <td className={cx(td, "tnum text-right font-medium")} data-label="Cost">
                              {usd(r.cost_usd)}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </div>
              ) : (
                <p className="px-4 pb-6 text-[13px] text-ink-3">No usage recorded in {month}.</p>
              )}
            </Card>
          </div>
        )}
      </Page>
    </>
  );
}
