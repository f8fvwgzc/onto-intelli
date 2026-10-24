import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, EyeOff } from "lucide-react";

import { Page } from "../../components/consoleUi";
import { Badge, Button, Checkbox, Dialog, Dot, EmptyState, ErrorState, Field, Segmented, Skeleton, Textarea } from "../../components/ui";
import { call, errorText } from "../../lib/api";
import { plural } from "../../lib/format";
import { useScope, useWhoami } from "../../lib/queries";
import { setQuery, useRoute } from "../../lib/router";
import type { LintFeedback, LintFinding, LintReport } from "../../lib/types";
import { Means, NotDone, SEVERITY_DOT } from "./common";

type Decision = "accept" | "dismiss";
type Filter = "all" | "error" | "warning" | "info";

export function LintTab() {
  const route = useRoute();
  const scope = useScope();
  const qc = useQueryClient();
  const me = useWhoami().data;
  const all = route.query.get("dismissed") === "1";
  const severity = (route.query.get("severity") as Filter) || "all";
  const [pending, setPending] = useState<{ finding: LintFinding; decision: Decision } | null>(null);
  const [note, setNote] = useState("");
  const q = useQuery<LintReport>({
    queryKey: ["lint", all, ...scope],
    queryFn: () => call<LintReport>("schema.lint", { include_dismissed: all }),
    placeholderData: (prev) => prev,
  });
  const feedback = useMutation({
    mutationFn: (v: { id: string; decision: Decision; note?: string }) => call<LintFeedback>("schema.lint_feedback", v),
    onSuccess: () => {
      setPending(null);
      void qc.invalidateQueries({ queryKey: ["lint"] });
    },
  });
  const canReview = Boolean(me?.capabilities.write);
  const open = (finding: LintFinding, decision: Decision) => {
    feedback.reset();
    setNote(finding.note ?? "");
    setPending({ finding, decision });
  };
  const r = q.data;
  const shown = r ? (severity === "all" ? r.findings : r.findings.filter((f) => f.severity === severity)) : [];

  return (
    <Page>
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        {r ? (
          <p className="tnum flex-1 text-[13px] text-ink-2">
            {plural(r.counts.error, "error")}, {plural(r.counts.warning, "warning")}, {plural(r.counts.info, "note")}
            {r.suppressed ? ` · ${r.suppressed} dismissed and hidden` : ""}
            {r.schema_version ? ` · schema v${r.schema_version}` : ""}
          </p>
        ) : (
          <span className="flex-1" />
        )}
        <Segmented<Filter>
          label="Severity"
          value={severity}
          onChange={(v) => setQuery({ severity: v === "all" ? undefined : v })}
          options={[
            { value: "all", label: "All" },
            { value: "error", label: "Errors" },
            { value: "warning", label: "Warnings" },
            { value: "info", label: "Notes" },
          ]}
        />
        <Checkbox label="Show dismissed" checked={all} onChange={(v) => setQuery({ dismissed: v ? "1" : undefined })} />
      </div>
      {q.error ? (
        <div className="mt-4">
          <ErrorState error={q.error} retry={() => void q.refetch()} />
        </div>
      ) : null}
      {q.isLoading ? <Skeleton className="mt-4 h-40" /> : null}
      {r ? (
        <div className="mt-4 flex flex-col gap-4">
          <NotDone title="Checks that did not run" items={(r.not_run ?? []).map((n) => `${n.check}: ${n.reason}`)} />
          <NotDone title="Checks that left part of the schema out" items={(r.partial ?? []).map((n) => `${n.check}: ${n.reason}`)} />
          {r.findings.length === 0 ? (
            <EmptyState title="No findings">
              {plural(r.checks_run.length, "check")} ran without a finding{r.suppressed ? `; ${plural(r.suppressed, "dismissed finding")} hidden` : ""}. Lint is a measurement of the schema's shape, not a proof that it means what you intend.
            </EmptyState>
          ) : shown.length === 0 ? (
            <EmptyState title="Nothing at this severity" />
          ) : (
            <ul className="flex flex-col divide-y divide-hairline rounded-card bg-raised shadow-card">
              {shown.map((f) => (
                <li key={f.id} className={f.dismissed ? "px-3 py-3 opacity-60" : "px-3 py-3"}>
                  <div className="flex flex-wrap items-start gap-x-3 gap-y-2">
                    <Dot color={SEVERITY_DOT[f.severity]} className="mt-1.5" />
                    <div className="min-w-0 flex-1">
                      <p className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[13px]">
                        <span className="font-medium text-ink">{f.message}</span>
                        {f.accepted ? <Badge tone="accent">accepted</Badge> : null}
                        {f.dismissed ? <Badge>dismissed</Badge> : null}
                      </p>
                      <p className="mt-0.5 text-[13px] text-ink-2">
                        <span className="text-ink-3">Fix: </span>
                        {f.fix}
                      </p>
                      <p className="mt-1 font-mono text-xs text-ink-3">
                        {f.severity} · {f.check} · {f.target} · {f.id}
                      </p>
                      {f.note ? <p className="mt-1 text-xs text-ink-2">Note: {f.note}</p> : null}
                    </div>
                    {canReview ? (
                      <div className="flex gap-1.5">
                        <Button size="sm" variant="ghost" icon={<Check className="size-3.5" aria-hidden />} disabled={f.accepted} onClick={() => open(f, "accept")}>
                          Accept
                        </Button>
                        <Button size="sm" variant="ghost" icon={<EyeOff className="size-3.5" aria-hidden />} disabled={f.dismissed} onClick={() => open(f, "dismiss")}>
                          Dismiss
                        </Button>
                      </div>
                    ) : null}
                  </div>
                </li>
              ))}
            </ul>
          )}
          {!canReview && r.findings.length ? <p className="text-xs text-ink-3">Accepting or dismissing a finding needs write access.</p> : null}
          <Means>{r.means}</Means>
        </div>
      ) : null}

      <Dialog
        open={pending !== null}
        onClose={() => setPending(null)}
        title={pending?.decision === "dismiss" ? "Dismiss finding" : "Accept finding"}
        footer={
          <>
            <Button onClick={() => setPending(null)}>Cancel</Button>
            <Button
              variant="primary"
              busy={feedback.isPending}
              onClick={() => pending && feedback.mutate({ id: pending.finding.id, decision: pending.decision, note: note.trim() || undefined })}
            >
              {pending?.decision === "dismiss" ? "Dismiss" : "Accept"}
            </Button>
          </>
        }
      >
        {pending ? (
          <div className="flex flex-col gap-3">
            <p className="text-[13px] text-ink-2">
              {pending.decision === "dismiss"
                ? "Dismissed findings leave the report and count as hidden; the decision is audited and the latest one wins."
                : "Accepted findings stay in the report, marked as right; the decision is audited."}
            </p>
            <p className="rounded-input bg-sunken px-2.5 py-2 text-[13px] text-ink">
              {pending.finding.message}
              <span className="mt-0.5 block font-mono text-xs text-ink-3">{pending.finding.target}</span>
            </p>
            <Field label="Note" hint="Optional: why, for the next reviewer">
              {(id) => <Textarea id={id} value={note} maxLength={2000} onChange={(e) => setNote(e.target.value)} />}
            </Field>
            {feedback.error ? <p className="text-[13px] text-danger">{errorText(feedback.error)}</p> : null}
          </div>
        ) : null}
      </Dialog>
    </Page>
  );
}
