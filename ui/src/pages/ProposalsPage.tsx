import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Bot, Check, X } from "lucide-react";

import { PlanVerdict, PlanView } from "../components/PlanView";
import { PageHeader } from "../components/Shell";
import { Badge, Button, EmptyState, ErrorState, Segmented, Spinner, Textarea, cx } from "../components/ui";
import { call, errorText } from "../lib/api";
import { ago, plural } from "../lib/format";
import { useScope, useWhoami } from "../lib/queries";
import { href, navigate, useRoute } from "../lib/router";
import type { Proposal } from "../lib/types";

type Status = "open" | "applied" | "rejected" | "failed" | "withdrawn" | "all";

const TONE: Record<Proposal["status"], "accent" | "neutral" | "danger" | "warning"> = {
  open: "accent",
  applied: "neutral",
  rejected: "neutral",
  failed: "danger",
  withdrawn: "neutral",
};

export function ProposalsPage() {
  const route = useRoute();
  const scope = useScope();
  const id = route.path[1];
  const status = (route.query.get("status") as Status) || "open";
  const list = useQuery<{ proposals: Proposal[] }>({
    queryKey: ["proposals", status, ...scope],
    queryFn: () => call("schema.proposals", { status: status === "all" ? undefined : status, limit: 200 }),
  });

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title="Proposals" context="Schema changes waiting for review, and what became of them" />
      <div className="flex min-h-0 flex-1 flex-col md:flex-row">
        <aside className={cx("flex min-h-0 flex-col border-hairline md:w-[340px] md:border-r", id && "hidden md:flex")}>
          <div className="overflow-x-auto p-3">
            <Segmented<Status>
              label="Status"
              value={status}
              onChange={(s) => navigate(["proposals", ...(id ? [id] : [])], { status: s === "open" ? undefined : s })}
              options={[
                { value: "open", label: "Open" },
                { value: "applied", label: "Applied" },
                { value: "failed", label: "Failed" },
                { value: "all", label: "All" },
              ]}
            />
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
            {list.isLoading ? (
              <div className="p-3"><Spinner /></div>
            ) : list.error ? (
              <div className="p-2"><ErrorState error={list.error} retry={() => list.refetch()} /></div>
            ) : list.data!.proposals.length === 0 ? (
              <EmptyState title={status === "open" ? "Nothing to review" : "No proposals here"}>
                Agents and people file schema changes here; approvers apply them.
              </EmptyState>
            ) : (
              <ul className="flex flex-col gap-0.5">
                {list.data!.proposals.map((p) => (
                  <li key={p.id}>
                    <a
                      href={href(["proposals", p.id], { status: status === "open" ? undefined : status })}
                      aria-current={id === p.id ? "page" : undefined}
                      className={cx("block rounded-input px-2.5 py-2", id === p.id ? "bg-sunken" : "hover:bg-sunken")}
                    >
                      <div className="flex items-center gap-2">
                        <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-ink">{p.title}</span>
                        {p.status !== "open" ? <Badge tone={TONE[p.status]}>{p.status}</Badge> : <PlanVerdict plan={p.plan} />}
                      </div>
                      <p className="mt-0.5 flex items-center gap-1 truncate text-xs text-ink-3">
                        {p.proposer.agent ? <Bot className="size-3.5 shrink-0" aria-label="Agent" /> : null}
                        {p.proposer.id} · {ago(p.created_at)} · {plural(p.change_set.changes.length, "change")}
                        {p.status === "open" ? ` · ${p.approvals}/${p.required_approvals} approvals` : ""}
                      </p>
                    </a>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </aside>
        <div className={cx("min-h-0 flex-1 overflow-y-auto", !id && "hidden md:block")}>
          {id ? <ProposalDetail id={id} /> : <EmptyState title="Pick a proposal">Its rationale, diff and impact on today's data appear here.</EmptyState>}
        </div>
      </div>
    </div>
  );
}

function ProposalDetail({ id }: { id: string }) {
  const scope = useScope();
  const qc = useQueryClient();
  const me = useWhoami().data;
  const [comment, setComment] = useState("");
  const q = useQuery<Proposal>({ queryKey: ["proposal", id, ...scope], queryFn: () => call<Proposal>("schema.proposal", { id }) });
  const act = useMutation({
    mutationFn: ({ action, seen }: { action: string; seen?: number }) => call(action, { id, comment, seen_version: seen }),
    onSuccess: () => {
      setComment("");
      void qc.invalidateQueries({ queryKey: ["proposal", id] });
      void qc.invalidateQueries({ queryKey: ["proposals"] });
      void qc.invalidateQueries({ queryKey: ["schema"] });
      void qc.invalidateQueries({ queryKey: ["overview"] });
    },
  });

  if (q.isLoading) return <div className="p-6"><Spinner /></div>;
  if (q.error) return <div className="p-5"><ErrorState error={q.error} retry={() => q.refetch()} /></div>;
  const p = q.data!;
  const plan = p.preview ?? p.plan;
  const open = p.status === "open";
  const mine = me?.principal.id === p.proposer.id;
  const canReview = !!me?.capabilities.approve && open;

  return (
    <article className="mx-auto w-full max-w-[820px] px-4 py-5 md:px-6">
      <a href={href(["proposals"])} className="text-[13px] text-ink-2 md:hidden">
        ← Proposals
      </a>
      <div className="flex flex-wrap items-start gap-3">
        <div className="min-w-0 flex-1">
          <h2 className="text-lg font-semibold tracking-tight">{p.title}</h2>
          <p className="mt-0.5 flex flex-wrap items-center gap-x-1.5 text-[13px] text-ink-2">
            {p.proposer.agent ? (
              <span className="inline-flex items-center gap-1">
                <Bot className="size-3.5" aria-hidden /> agent
              </span>
            ) : null}
            <span className="font-medium text-ink">{p.proposer.id}</span>
            <span>proposed {ago(p.created_at)}</span>
            {p.source ? <span>· via {p.source}</span> : null}
            {p.revision > 1 ? <span>· revision {p.revision}</span> : null}
          </p>
        </div>
        <Badge tone={TONE[p.status]}>{p.status}</Badge>
      </div>

      {p.rationale ? <p className="mt-4 max-w-[65ch] text-[14px] leading-relaxed whitespace-pre-wrap text-ink">{p.rationale}</p> : null}

      {p.status === "applied" ? (
        <p className="mt-4 text-[13px] text-ink-2">
          Applied as schema version {p.applied_version}. <a className="text-accent underline-offset-2 hover:underline" href={href(["schema", "history"])}>See versions</a>
        </p>
      ) : null}
      {p.error ? <p className="mt-4 rounded-card bg-danger-tint p-3 text-[13px] text-ink">{p.error}</p> : null}
      {p.stale && open ? (
        <p className="mt-4 rounded-card bg-warning-tint p-3 text-[13px] text-ink">
          Filed against schema v{p.base_version}; the schema is now v{p.active_version}. The impact below is recomputed against today's schema and data.
        </p>
      ) : null}

      <section className="mt-6">
        <h3 className="mb-3 text-[13px] font-semibold">Impact</h3>
        <PlanView plan={plan} />
      </section>

      <section className="mt-8">
        <h3 className="mb-2 text-[13px] font-semibold">
          Reviews <span className="tnum font-normal text-ink-3">· {p.approvals}/{p.required_approvals} approvals</span>
        </h3>
        {p.reviews.length === 0 ? <p className="text-[13px] text-ink-3">No reviews yet.</p> : null}
        <ol className="flex flex-col gap-2">
          {p.reviews.map((r, i) => (
            <li key={i} className="flex gap-2 text-[13px]">
              <span className={cx("mt-1.5 size-1.5 shrink-0 rounded-full", r.decision === "approve" ? "bg-success" : r.decision === "reject" ? "bg-danger" : "bg-[var(--gray-3)]")} aria-hidden />
              <div>
                <p>
                  <span className="font-medium">{r.by}</span> <span className="text-ink-2">{r.decision === "approve" ? "approved" : r.decision === "reject" ? "rejected" : "commented"}</span>{" "}
                  <span className="text-ink-3">{ago(r.ts)}</span>
                </p>
                {r.comment ? <p className="text-ink-2">{r.comment}</p> : null}
              </div>
            </li>
          ))}
        </ol>
      </section>

      {open ? (
        <section className="mt-6 rounded-card bg-raised p-4 shadow-card">
          <Textarea aria-label="Comment" placeholder={canReview ? "Comment (optional for approve and reject)" : "Comment"} value={comment} onChange={(e) => setComment(e.target.value)} />
          <div className="mt-3 flex flex-wrap items-center gap-2">
            {act.error ? <p className="mr-auto text-[13px] text-danger">{errorText(act.error)}</p> : null}
            {!canReview && me && !me.capabilities.approve ? <p className="mr-auto text-[13px] text-ink-3">Approvers decide; you can comment.</p> : null}
            {mine ? (
              <Button size="sm" variant="ghost" disabled={act.isPending} onClick={() => act.mutate({ action: "schema.withdraw" })}>
                Withdraw
              </Button>
            ) : null}
            <Button size="sm" disabled={!comment.trim() || act.isPending} onClick={() => act.mutate({ action: "schema.comment" })}>
              Comment
            </Button>
            {canReview ? (
              <>
                <Button size="sm" variant="danger" icon={<X className="size-4" />} disabled={act.isPending} onClick={() => act.mutate({ action: "schema.reject" })}>
                  Reject
                </Button>
                <Button
                  size="sm"
                  variant="primary"
                  icon={<Check className="size-4" />}
                  busy={act.isPending}
                  disabled={mine || !plan.valid || !!(plan.needs_migration ?? []).length}
                  title={mine ? "You cannot approve your own proposal" : undefined}
                  onClick={() => act.mutate({ action: "schema.approve", seen: p.active_version })}
                >
                  {p.approvals + 1 >= p.required_approvals ? "Approve and apply" : "Approve"}
                </Button>
              </>
            ) : null}
          </div>
        </section>
      ) : null}
    </article>
  );
}
