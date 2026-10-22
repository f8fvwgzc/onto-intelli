import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, ThumbsDown, ThumbsUp } from "lucide-react";

import { Card, Page, RoleButton, Status } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { EmptyState, ErrorState, Field, Input, Panel, Skeleton, Textarea, cx } from "../../components/ui";
import { ApiError, errorText } from "../../lib/api";
import { useConsoleScope } from "../../lib/console";
import { ago, ms, num, usd, when } from "../../lib/format";
import { useScope } from "../../lib/queries";
import { observe, type ScoreRow, type SpanRow, type TraceDetail } from "../../lib/observe";
import { href } from "../../lib/router";

const KINDS = ["span", "generation", "retrieval", "tool", "event"];

function Json({ value }: { value: unknown }) {
  if (value === null || value === undefined) return <p className="text-[13px] text-ink-3">None</p>;
  const text = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  return <pre className="max-h-72 overflow-auto rounded-input bg-sunken p-2.5 font-mono text-[12px] leading-relaxed break-words whitespace-pre-wrap text-ink">{text}</pre>;
}

function Meta({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-ink-3">{label}</dt>
      <dd className="tnum truncate text-[13px] text-ink">{children}</dd>
    </div>
  );
}

interface Row {
  span: SpanRow;
  depth: number;
  offset: number;
}

/** Spans in tree order (children after their parent, by start) with depth and offset. */
function layout(d: TraceDetail): { rows: Row[]; total: number } {
  const ids = new Set(d.spans.map((s) => s.id));
  const kids = new Map<string, SpanRow[]>();
  const roots: SpanRow[] = [];
  for (const s of d.spans) {
    if (s.parent_id && ids.has(s.parent_id) && s.parent_id !== s.id) {
      const list = kids.get(s.parent_id) ?? [];
      list.push(s);
      kids.set(s.parent_id, list);
    } else roots.push(s);
  }
  const byStart = (a: SpanRow, b: SpanRow) => a.ts - b.ts || (a.id < b.id ? -1 : 1);
  const rows: Row[] = [];
  const seen = new Set<string>();
  const walk = (s: SpanRow, depth: number) => {
    if (seen.has(s.id)) return;
    seen.add(s.id);
    rows.push({ span: s, depth, offset: Math.max(0, s.ts - d.trace.ts) });
    for (const c of (kids.get(s.id) ?? []).sort(byStart)) walk(c, depth + 1);
  };
  for (const r of roots.sort(byStart)) walk(r, 0);
  const total = Math.max(d.trace.duration_ms, ...rows.map((r) => r.offset + r.span.duration_ms), 1);
  return { rows, total };
}

function Waterfall({ detail, selected, onSelect }: { detail: TraceDetail; selected?: string; onSelect: (id: string) => void }) {
  const { rows, total } = useMemo(() => layout(detail), [detail]);
  const ticks = [0, 0.25, 0.5, 0.75, 1];
  return (
    <div className="overflow-x-auto overscroll-x-contain">
      <div className="min-w-[600px]">
        <div className="flex h-7 items-end border-b border-hairline text-[11px] text-ink-3">
          <div className="flex w-[260px] shrink-0 justify-between px-2 pb-1">
            <span>Span</span>
            <span>Duration</span>
          </div>
          <div className="relative mr-3 h-full flex-1">
            {ticks.map((t) => (
              <span
                key={t}
                className="tnum absolute bottom-1 whitespace-nowrap"
                style={{ left: `${t * 100}%`, transform: t === 1 ? "translateX(-100%)" : t ? "translateX(-50%)" : undefined }}
              >
                {ms(total * t)}
              </span>
            ))}
          </div>
        </div>
        <ul role="listbox" aria-label="Spans" className="relative">
          {rows.map(({ span: s, depth, offset }) => {
            const left = (offset / total) * 100;
            const width = Math.max(0.4, (s.duration_ms / total) * 100);
            const color = `var(--kind-${KINDS.includes(s.kind) ? s.kind : "span"})`;
            const active = selected === s.id;
            return (
              <li key={s.id} role="option" aria-selected={active}>
                <button
                  onClick={() => onSelect(s.id)}
                  className={cx("flex h-8 w-full items-center text-left hover:bg-sunken focus-visible:bg-accent-tint focus-visible:outline-none", active && "bg-accent-tint")}
                  aria-label={`${s.name}, ${s.kind}, ${ms(s.duration_ms)}, starts at ${ms(offset)}${s.status === "error" ? ", error" : ""}`}
                >
                  <span className="flex w-[260px] shrink-0 items-center gap-1.5 px-2 text-[12.5px]" style={{ paddingLeft: 8 + depth * 14 }}>
                    <span aria-hidden className="inline-block size-2 shrink-0 rounded-[2px]" style={{ background: color }} />
                    <span className={cx("min-w-0 flex-1 truncate font-mono", s.status === "error" ? "text-danger" : "text-ink")}>{s.name}</span>
                    <span className="tnum shrink-0 text-[11px] text-ink-3">{s.kind === "event" ? "" : ms(s.duration_ms)}</span>
                  </span>
                  <span className="relative mr-3 h-full flex-1">
                    {ticks.slice(1, -1).map((t) => (
                      <span key={t} aria-hidden className="absolute inset-y-0 w-px bg-hairline" style={{ left: `${t * 100}%` }} />
                    ))}
                    {s.kind === "event" ? (
                      <span
                        aria-hidden
                        className="absolute top-1/2 size-2 -translate-x-1/2 -translate-y-1/2 rotate-45 rounded-[1px]"
                        style={{ left: `${left}%`, background: color }}
                      />
                    ) : (
                      <span
                        aria-hidden
                        className="absolute top-1/2 h-3 -translate-y-1/2 rounded-[3px]"
                        style={{
                          left: `${left}%`,
                          width: `${Math.min(width, 100 - left)}%`,
                          minWidth: 2,
                          background: color,
                          opacity: s.status === "error" ? 1 : 0.85,
                          outline: s.status === "error" ? "1.5px solid var(--danger)" : undefined,
                        }}
                      />
                    )}
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
        <ul className="mt-2 flex flex-wrap gap-3 px-2 text-[11.5px] text-ink-3">
          {KINDS.map((k) => (
            <li key={k} className="inline-flex items-center gap-1.5">
              <span aria-hidden className="inline-block size-2 rounded-[2px]" style={{ background: `var(--kind-${k})` }} />
              {k}
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}

function SpanPanel({ span, onClose }: { span?: SpanRow; onClose: () => void }) {
  return (
    <Panel
      open={Boolean(span)}
      onClose={onClose}
      wide
      title={
        span ? (
          <div className="min-w-0">
            <p className="truncate font-mono text-[13px] font-medium text-ink">{span.name}</p>
            <p className="text-xs text-ink-3">
              {span.kind} · {ms(span.duration_ms)}
            </p>
          </div>
        ) : null
      }
    >
      {span ? (
        <div className="flex flex-col gap-4 pt-3">
          {span.error ? <p className="rounded-input bg-danger-tint p-2.5 text-[13px] text-danger">{span.error}</p> : null}
          <dl className="grid grid-cols-2 gap-3">
            <Meta label="Status">{span.status}</Meta>
            <Meta label="Started">{when(span.ts)}</Meta>
            {span.model ? <Meta label="Model">{span.model}</Meta> : null}
            {span.input_tokens || span.output_tokens ? (
              <Meta label="Tokens in · out · cached">
                {num(span.input_tokens)} · {num(span.output_tokens)} · {num(span.cached_tokens)}
              </Meta>
            ) : null}
            {span.cost_usd ? <Meta label="Cost">{usd(span.cost_usd)}</Meta> : null}
            <Meta label="Span id">
              <span className="font-mono text-[12px]">{span.id}</span>
            </Meta>
          </dl>
          <section>
            <h3 className="mb-1.5 text-[13px] font-semibold">Attributes</h3>
            <Json value={Object.keys(span.attrs ?? {}).length ? span.attrs : null} />
          </section>
          <section>
            <h3 className="mb-1.5 text-[13px] font-semibold">Input</h3>
            <Json value={span.input} />
          </section>
          <section>
            <h3 className="mb-1.5 text-[13px] font-semibold">Output</h3>
            <Json value={span.output} />
          </section>
        </div>
      ) : null}
    </Panel>
  );
}

function Scores({ id, scores }: { id: string; scores: ScoreRow[] }) {
  const qc = useQueryClient();
  const scope = useScope();
  const { role } = useConsoleScope();
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [comment, setComment] = useState("");
  const add = useMutation({
    mutationFn: (s: { name: string; value?: number; label?: string; comment?: string }) => observe.score(id, s),
    onSuccess: () => {
      setName("");
      setValue("");
      setComment("");
      return qc.invalidateQueries({ queryKey: ["trace", ...scope, id] });
    },
  });
  const custom = () => {
    const n = Number(value);
    add.mutate({ name: name.trim(), ...(value.trim() !== "" && Number.isFinite(n) ? { value: n } : { label: value.trim() }), comment: comment.trim() || undefined });
  };
  return (
    <Card title="Scores" aside={<span className="text-xs text-ink-3">Evaluations and feedback</span>}>
      {scores.length ? (
        <ul className="mb-4 flex flex-col divide-y divide-hairline">
          {scores.map((s) => (
            <li key={s.id} className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 py-2 text-[13px]">
              <span className="font-mono text-ink">{s.name}</span>
              <span className="tnum font-medium">{s.value ?? s.label}</span>
              <span className="text-xs text-ink-3">
                {s.source}
                {s.author ? ` · ${s.author}` : ""} · {ago(s.ts)}
              </span>
              {s.comment ? <p className="basis-full text-ink-2">{s.comment}</p> : null}
            </li>
          ))}
        </ul>
      ) : (
        <p className="mb-4 text-[13px] text-ink-3">No scores yet.</p>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <RoleButton
          size="sm"
          role={role}
          min="developer"
          icon={<ThumbsUp className="size-4" aria-hidden />}
          busy={add.isPending && add.variables?.name === "user.feedback" && add.variables.value === 1}
          onClick={() => add.mutate({ name: "user.feedback", value: 1 })}
        >
          Good
        </RoleButton>
        <RoleButton
          size="sm"
          role={role}
          min="developer"
          icon={<ThumbsDown className="size-4" aria-hidden />}
          busy={add.isPending && add.variables?.name === "user.feedback" && add.variables.value === 0}
          onClick={() => add.mutate({ name: "user.feedback", value: 0 })}
        >
          Bad
        </RoleButton>
      </div>
      <form
        className="mt-3 grid gap-2 sm:grid-cols-[1fr_120px]"
        onSubmit={(e) => {
          e.preventDefault();
          if (name.trim() && value.trim()) custom();
        }}
      >
        <Field label="Score name">{(fid) => <Input id={fid} value={name} onChange={(e) => setName(e.target.value)} placeholder="accuracy" maxLength={100} />}</Field>
        <Field label="Value or label">{(fid) => <Input id={fid} value={value} onChange={(e) => setValue(e.target.value)} placeholder="0.8" maxLength={100} />}</Field>
        <div className="sm:col-span-2">
          <Field label="Comment">{(fid) => <Textarea id={fid} value={comment} onChange={(e) => setComment(e.target.value)} maxLength={2000} className="min-h-14" />}</Field>
        </div>
        <div className="flex items-center gap-3 sm:col-span-2">
          <RoleButton
            type="submit"
            size="sm"
            variant="primary"
            role={role}
            min="developer"
            disabled={!name.trim() || !value.trim()}
            busy={add.isPending && add.variables?.name !== "user.feedback"}
          >
            Add score
          </RoleButton>
          {add.error ? <p className="text-xs text-danger">{errorText(add.error)}</p> : null}
        </div>
      </form>
    </Card>
  );
}

export default function TracePage({ id }: { id: string }) {
  const scope = useScope();
  const [sel, setSel] = useState<string>();
  const q = useQuery({ queryKey: ["trace", ...scope, id], queryFn: () => observe.trace(id), retry: (n, e) => !(e instanceof ApiError && e.status < 500) && n < 2 });
  const d = q.data;
  const t = d?.trace;
  const span = d?.spans.find((s) => s.id === sel);

  return (
    <>
      <PageHeader title={t ? t.name : "Trace"} context={<span className="font-mono">{id}</span>}>
        <a href={href(["traces"])} className="inline-flex h-8 items-center gap-1.5 rounded-input px-2.5 text-[13px] font-medium text-ink-2 hover:bg-sunken hover:text-ink">
          <ArrowLeft className="size-4" aria-hidden /> Traces
        </a>
      </PageHeader>
      <div className="flex min-h-0 flex-1">
        <Page>
          {q.error ? (
            q.error instanceof ApiError && q.error.status === 404 ? (
              <EmptyState title="Trace not found">It may be older than this project's retention, or still being written. Try again in a moment.</EmptyState>
            ) : (
              <ErrorState error={q.error} retry={() => void q.refetch()} />
            )
          ) : !d || !t ? (
            <div className="flex flex-col gap-3">
              <Skeleton className="h-24 rounded-card" />
              <Skeleton className="h-64 rounded-card" />
            </div>
          ) : (
            <div className="flex flex-col gap-3">
              <Card>
                <dl className="grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-4 xl:grid-cols-8">
                  <Meta label="Status">
                    <Status tone={t.status === "error" ? "error" : "ok"}>{t.status === "error" ? (t.error_code ?? "error") : "OK"}</Status>
                  </Meta>
                  <Meta label="Latency">{ms(t.duration_ms)}</Meta>
                  <Meta label="Tokens in · out">
                    {num(t.input_tokens)} · {num(t.output_tokens)}
                  </Meta>
                  <Meta label="Cost">{usd(t.cost_usd)}</Meta>
                  <Meta label="Mode">{t.mode ?? t.kind}</Meta>
                  <Meta label="User">{t.user_id ?? t.principal}</Meta>
                  <Meta label="Session">
                    {t.session_id ? (
                      <a className="text-accent hover:underline" href={href(["sessions", t.session_id])}>
                        {t.session_id}
                      </a>
                    ) : (
                      "–"
                    )}
                  </Meta>
                  <Meta label="API key">
                    <span className="font-mono text-[12px]">{t.api_key ?? "–"}</span>
                  </Meta>
                </dl>
                <p className="mt-3 text-xs text-ink-3">
                  {when(t.ts)} · {t.source} · workspace {t.workspace}
                  {t.model ? ` · ${t.model}` : ""}
                  {t.tags.length ? ` · ${t.tags.join(", ")}` : ""}
                </p>
                {t.error_message ? <p className="mt-2 rounded-input bg-danger-tint p-2.5 text-[13px] text-danger">{t.error_message}</p> : null}
              </Card>
              {t.input?.question || t.output?.answer ? (
                <Card title="Question and answer">
                  {t.input?.question ? <p className="text-[13px] font-medium text-ink">{t.input.question}</p> : null}
                  {t.output?.answer ? <p className="mt-2 text-[13px] whitespace-pre-wrap text-ink-2">{t.output.answer}</p> : null}
                </Card>
              ) : null}
              <Card title="Spans" aside={<span className="text-xs text-ink-3">{d.spans.length} spans · select one for its details</span>} pad={false} className="pb-3">
                <Waterfall detail={d} selected={sel} onSelect={(sid) => setSel((cur) => (cur === sid ? undefined : sid))} />
              </Card>
              <Scores id={id} scores={d.scores} />
            </div>
          )}
        </Page>
        <SpanPanel span={span} onClose={() => setSel(undefined)} />
      </div>
    </>
  );
}
