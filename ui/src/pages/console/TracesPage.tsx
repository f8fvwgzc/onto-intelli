import { useEffect, useMemo, useRef, useState } from "react";
import { useInfiniteQuery } from "@tanstack/react-query";
import { Radio, Search, SlidersHorizontal, X } from "lucide-react";

import { Histogram } from "../../components/charts";
import { Page, Status, td, th } from "../../components/consoleUi";
import { PageHeader, isTyping } from "../../components/Shell";
import { Button, EmptyState, ErrorState, Input, Kbd, Select, Skeleton, cx } from "../../components/ui";
import { useConsoleScope } from "../../lib/console";
import { ms, num, usd, when } from "../../lib/format";
import { useScope } from "../../lib/queries";
import { RANGES, observe, rangeMs, type TraceFilter, type TraceRow } from "../../lib/observe";
import { href, navigate, setQuery, useRoute } from "../../lib/router";
import { pickFilters, searchesQuestions } from "../../lib/traceSearch";
import { minuteNow } from "./ProjectsPage";

const TEXT_FILTERS = [
  { key: "name", label: "Name", placeholder: "intelli.ask" },
  { key: "user_id", label: "User", placeholder: "user id" },
  { key: "session_id", label: "Session", placeholder: "session id" },
  { key: "model", label: "Model", placeholder: "claude-…" },
  { key: "tag", label: "Tag", placeholder: "tag" },
] as const;

const FILTER_KEYS = ["status", "mode", "q", ...TEXT_FILTERS.map((t) => t.key)];

const MODES = ["deterministic", "llm", "llm_cache", "answer_cache", "fallback", "clarify", "proposal", "action"];

const short = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" });

/** A filter text box that applies on Enter or when it loses focus. */
function FilterInput({ name, label, placeholder, value }: { name: string; label: string; placeholder: string; value: string }) {
  const [v, setV] = useState(value);
  useEffect(() => setV(value), [value]);
  return (
    <label className="flex min-w-0 flex-col gap-1 text-xs text-ink-3">
      {label}
      <Input
        value={v}
        placeholder={placeholder}
        onChange={(e) => setV(e.target.value)}
        onBlur={() => v.trim() !== value && setQuery({ [name]: v.trim() || undefined })}
        onKeyDown={(e) => e.key === "Enter" && setQuery({ [name]: v.trim() || undefined })}
        className="h-8 text-[13px]"
      />
    </label>
  );
}

export default function TracesPage() {
  const route = useRoute();
  const scope = useScope();
  // Only admins search questions (the server refuses `q` to others).
  const { role } = useConsoleScope();
  const canSearch = searchesQuestions(role);
  const q = route.query;
  const range = q.get("range") ?? "24h";
  const live = q.get("live") === "1";
  const search = useRef<HTMLInputElement>(null);
  const rows = useRef<(HTMLTableRowElement | null)[]>([]);
  const [text, setText] = useState(q.get("q") ?? "");
  const [more, setMore] = useState(() => TEXT_FILTERS.some((f) => q.get(f.key)));
  const [sel, setSel] = useState(-1);

  // Search applies 300 ms after typing stops.
  useEffect(() => {
    const t = window.setTimeout(() => (text.trim() || undefined) !== (q.get("q") ?? undefined) && setQuery({ q: text.trim() || undefined }), 300);
    return () => window.clearTimeout(t);
  }, [text, q]);

  const filter: TraceFilter = useMemo(() => {
    const f: TraceFilter = { limit: 50, ...pickFilters(q, FILTER_KEYS, role) };
    if (range !== "all") f.since = minuteNow() - rangeMs(range, "24h");
    return f;
    // A new minute moves the window; the query string and the role are the rest.
  }, [q.toString(), range, role]);

  const traces = useInfiniteQuery({
    queryKey: ["traces", ...scope, filter],
    queryFn: ({ pageParam }) => observe.traces({ ...filter, cursor: pageParam || undefined }),
    initialPageParam: "",
    getNextPageParam: (last) => last.next ?? undefined,
    refetchInterval: live ? 10_000 : false,
    placeholderData: (p) => p,
  });
  const list: TraceRow[] = useMemo(() => traces.data?.pages.flatMap((p) => p.traces) ?? [], [traces.data]);

  // Keyboard: / focuses the search, j/k move through rows (Enter on a row opens it).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
      if (e.key === "/" && canSearch && !isTyping(e.target)) {
        e.preventDefault();
        search.current?.focus();
        return;
      }
      if (isTyping(e.target) || document.querySelector("dialog[open]")) return;
      if (e.key === "j" || e.key === "k") {
        e.preventDefault();
        const next = Math.max(0, Math.min(list.length - 1, sel + (e.key === "j" ? 1 : -1)));
        // Focusing the row selects it (onFocus).
        rows.current[next]?.focus();
      }
    };
    // Capture phase: this page's "/" wins over the shell's (which opens the palette).
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [list, sel, canSearch]);

  const active = Object.keys(pickFilters(q, FILTER_KEYS, role)).length > 0;

  return (
    <>
      <PageHeader title="Traces" context="Every ask, governed action and LLM-using call of this project">
        <Button
          size="sm"
          variant={live ? "primary" : "secondary"}
          aria-pressed={live}
          icon={<Radio className={cx("size-4", live && "motion-safe:animate-pulse")} aria-hidden />}
          onClick={() => setQuery({ live: live ? undefined : "1" })}
          title="Refresh every 10 seconds"
        >
          Live
        </Button>
      </PageHeader>
      <div className="flex flex-col gap-2 border-b border-hairline px-4 py-2.5 md:px-5">
        <div className="flex flex-wrap items-center gap-2">
          {canSearch ? (
            <div className="relative min-w-0 flex-1 basis-56">
              <Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-ink-3" aria-hidden />
              <Input
                ref={search}
                aria-label="Search questions"
                placeholder="Search questions"
                value={text}
                onChange={(e) => setText(e.target.value)}
                className="h-8 pr-8 pl-8 text-[13px]"
                onKeyDown={(e) => e.key === "Escape" && (e.currentTarget.blur(), e.preventDefault())}
              />
              <span className="pointer-events-none absolute top-1/2 right-2 hidden -translate-y-1/2 sm:block">
                <Kbd>/</Kbd>
              </span>
            </div>
          ) : null}
          <Select
            aria-label="Time range"
            value={range}
            onChange={(e) => setQuery({ range: e.target.value === "24h" ? undefined : e.target.value })}
            className="h-8 max-w-44 text-[13px]"
          >
            {RANGES.map((r) => (
              <option key={r.value} value={r.value}>
                {r.label}
              </option>
            ))}
            <option value="all">All time</option>
          </Select>
          <Select aria-label="Status" value={q.get("status") ?? ""} onChange={(e) => setQuery({ status: e.target.value || undefined })} className="h-8 max-w-44 text-[13px]">
            <option value="">Any status</option>
            <option value="ok">OK</option>
            <option value="error">Error</option>
          </Select>
          <Select aria-label="Mode" value={q.get("mode") ?? ""} onChange={(e) => setQuery({ mode: e.target.value || undefined })} className="h-8 max-w-44 text-[13px]">
            <option value="">Any mode</option>
            {MODES.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </Select>
          <Button size="sm" variant="ghost" aria-expanded={more} icon={<SlidersHorizontal className="size-4" aria-hidden />} onClick={() => setMore((m) => !m)}>
            Filters
          </Button>
          {active ? (
            <Button
              size="sm"
              variant="ghost"
              icon={<X className="size-4" aria-hidden />}
              onClick={() => {
                setText("");
                setQuery(Object.fromEntries(FILTER_KEYS.map((k) => [k, undefined])));
              }}
            >
              Reset
            </Button>
          ) : null}
        </div>
        {more ? (
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-5">
            {TEXT_FILTERS.map((f) => (
              <FilterInput key={f.key} name={f.key} label={f.label} placeholder={f.placeholder} value={q.get(f.key) ?? ""} />
            ))}
          </div>
        ) : null}
      </div>
      <Page className="pt-3 md:pt-3">
        {traces.error ? (
          <ErrorState error={traces.error} retry={() => void traces.refetch()} />
        ) : traces.isLoading ? (
          <div className="flex flex-col gap-2">
            {[0, 1, 2, 3, 4, 5].map((i) => (
              <Skeleton key={i} className="h-10" />
            ))}
          </div>
        ) : !list.length ? (
          <EmptyState title={active ? "No traces match these filters" : "No traces in this range"}>
            {active ? (
              "Loosen a filter or pick a longer range."
            ) : (
              <>
                Traces appear as requests arrive.{" "}
                <a href={href(["connect"])} className="text-accent hover:underline">
                  Connect a service
                </a>{" "}
                to send the first one.
              </>
            )}
          </EmptyState>
        ) : (
          <>
            <div className="mb-3 flex items-end gap-4">
              <div className="min-w-0 flex-1">
                <p className="mb-1 text-xs text-ink-3">Latency of the {num(list.length)} traces shown</p>
                <Histogram values={list.map((t) => t.duration_ms)} label={`Latency distribution of ${list.length} traces`} height={40} />
              </div>
              <p className="hidden shrink-0 text-xs text-ink-3 sm:block">
                <Kbd>j</Kbd> <Kbd>k</Kbd> move · <Kbd>↵</Kbd> open
              </p>
            </div>
            {/* The fixed columns take about 750 px; the question keeps at least 200 px and the table scrolls sideways when narrower. */}
            <div className="overflow-x-auto overscroll-x-contain">
            <table className="stack-table w-full table-fixed border-collapse text-[13px] md:min-w-[960px]">
              <thead>
                <tr className="border-b border-hairline">
                  <th className={cx(th, "w-[136px]")}>Time</th>
                  <th className={cx(th, "w-[150px]")}>Name</th>
                  <th className={th}>Question</th>
                  <th className={cx(th, "w-[110px]")}>Mode</th>
                  <th className={cx(th, "w-[84px] text-right")}>Latency</th>
                  <th className={cx(th, "w-[72px] text-right")}>Tokens</th>
                  <th className={cx(th, "w-[76px] text-right")}>Cost</th>
                  <th className={cx(th, "w-[120px]")}>User</th>
                </tr>
              </thead>
              <tbody>
                {list.map((t, i) => (
                  <tr
                    key={t.id}
                    ref={(el) => {
                      rows.current[i] = el;
                    }}
                    tabIndex={i === Math.max(0, sel) ? 0 : -1}
                    aria-selected={i === sel}
                    onFocus={() => setSel(i)}
                    onClick={() => navigate(["traces", t.id])}
                    onKeyDown={(e) => e.key === "Enter" && navigate(["traces", t.id])}
                    className={cx("cursor-pointer border-b border-hairline outline-none hover:bg-sunken focus-visible:bg-accent-tint", i === sel && "bg-sunken")}
                  >
                    <td className={cx(td, "tnum whitespace-nowrap text-ink-2")} data-label="Time" title={when(t.ts)}>
                      {short.format(t.ts)}
                    </td>
                    <td className={td} data-primary>
                      <a href={href(["traces", t.id])} tabIndex={-1} onClick={(e) => e.stopPropagation()} className="inline-flex max-w-full">
                        <Status tone={t.status === "error" ? "error" : "ok"}>
                          <span className="truncate font-mono text-[12.5px] text-ink">{t.name}</span>
                        </Status>
                      </a>
                    </td>
                    <td className={cx(td, "text-ink-2")} data-label="Question">
                      <span className="block truncate">{t.preview ?? (t.error_code ? <span className="text-danger">{t.error_code}</span> : "–")}</span>
                    </td>
                    <td className={cx(td, "truncate text-ink-2")} data-label="Mode">
                      {t.mode ?? t.kind}
                    </td>
                    <td className={cx(td, "tnum text-right whitespace-nowrap")} data-label="Latency">
                      {ms(t.duration_ms)}
                    </td>
                    <td className={cx(td, "tnum text-right text-ink-2")} data-label="Tokens">
                      {t.input_tokens + t.output_tokens ? num(t.input_tokens + t.output_tokens) : "–"}
                    </td>
                    <td className={cx(td, "tnum text-right text-ink-2")} data-label="Cost">
                      {t.cost_usd ? usd(t.cost_usd) : "–"}
                    </td>
                    <td className={cx(td, "text-ink-2")} data-label="User">
                      <span className="block truncate">{t.user_id ?? t.principal}</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            </div>
            <div className="mt-4 flex justify-center">
              {traces.hasNextPage ? (
                <Button size="sm" busy={traces.isFetchingNextPage} onClick={() => void traces.fetchNextPage()}>
                  Load more
                </Button>
              ) : (
                <p className="text-xs text-ink-3">{list.length > 50 ? "That is every trace in this range." : null}</p>
              )}
            </div>
          </>
        )}
      </Page>
    </>
  );
}
