// Automations: what runs when the ontology changes, as whom, and what each
// run did. Definitions live in the schema and are visible to every schema
// reader; approvers who cover an automation's principal enable it, and an
// enablement stops when anything it reaches changes or when it expires.

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CirclePause, CirclePlay, FlaskConical, RotateCcw, TriangleAlert } from "lucide-react";

import { CodeBlock, Status, td, th } from "../../components/consoleUi";
import { Badge, Button, EmptyState, ErrorState, Field, Input, Panel, Section, Segmented, Skeleton, cx } from "../../components/ui";
import { call, errorText } from "../../lib/api";
import { ago, plural, when } from "../../lib/format";
import { useScope, useWhoami } from "../../lib/queries";
import { href, navigate } from "../../lib/router";
import type { AutomationItem, AutomationList, AutomationRun, AutomationRuns, RunState } from "../../lib/types";

const RUN_TONE: Record<RunState, "ok" | "error" | "warn" | "muted"> = {
  running: "muted",
  succeeded: "ok",
  failed: "error",
  retry: "warn",
  skipped: "warn",
  refused: "error",
  would_run: "muted",
  cancelled: "muted",
};

const RUN_LABEL: Record<RunState, string> = {
  running: "Running",
  succeeded: "Succeeded",
  failed: "Failed",
  retry: "Retrying",
  skipped: "Skipped",
  refused: "Refused",
  would_run: "Would run",
  cancelled: "Cancelled",
};

/** Why an automation does not run, in words. */
function inactiveText(reason: string | null): string {
  if (!reason) return "Inactive";
  const r = reason.replace(/^paused: /, "");
  if (r === "not_enabled") return "Not enabled";
  if (r === "killed") return "Stopped by the kill switch";
  if (r === "enablement_expired") return "Enablement expired";
  if (r.startsWith("stale_definition")) return "Changed since enabled";
  if (r.startsWith("failing")) return "Paused, failing";
  if (r === "not_defined") return "Not defined";
  return r;
}

function AutomationStatus({ a }: { a: AutomationItem }) {
  if (a.status === "active") return <Status tone="ok">{a.mode === "shadow" ? "Active, shadow" : "Active"}</Status>;
  const tone = a.reason === "not_enabled" ? "muted" : "warn";
  return <Status tone={tone}>{inactiveText(a.reason)}</Status>;
}

/** The trigger as a sentence: "issue updated, state → blocked". */
function triggerText(a: AutomationItem): string {
  const t = a.trigger;
  const what = t.kinds?.length ? t.kinds.join(", ") : "anything";
  const ops = t.ops?.length ? t.ops.map((o) => o.replace("entity.", "").replace("link.", "link ")).join(" or ") : "changed";
  const parts = [`${what} ${ops}`];
  if (t.state?.to?.length) parts.push(`state → ${t.state.to.join(" | ")}`);
  if (t.state?.from?.length) parts.push(`from ${t.state.from.join(" | ")}`);
  if (t.props?.length) parts.push(`touching ${t.props.join(", ")}`);
  if (t.rels?.length) parts.push(`via ${t.rels.join(", ")}`);
  return parts.join(", ");
}

function doesText(a: AutomationItem): string {
  return a.effects.map((e) => (e.kind === "action" ? `runs ${e.target}` : `tells ${e.target}`)).join(" · ");
}

function runsAs(a: AutomationItem): string {
  const ws = a.run_as.workspaces.includes("*") ? "every workspace" : a.run_as.workspaces.join(", ");
  return `${a.run_as.roles.join(" + ")} in ${ws}`;
}

export function AutomationsTab({ name }: { name?: string }) {
  const scope = useScope();
  const qc = useQueryClient();
  const me = useWhoami().data;
  const admin = me?.capabilities.configure_outbound ?? false;
  const list = useQuery<AutomationList>({
    queryKey: ["automations", ...scope],
    queryFn: () => call<AutomationList>("automation.list"),
    refetchInterval: 15_000,
  });
  const kill = useMutation({
    mutationFn: (on: boolean) => call("automation.kill", on ? { on: true } : { off: true }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["automations"] }),
  });
  const close = () => navigate(["automate", "automations"]);
  const data = list.data;
  const selected = data?.items.find((a) => a.name === name);
  const active = data?.items.filter((a) => a.status === "active").length ?? 0;

  return (
    <div className="flex min-h-0 flex-1 flex-col md:flex-row">
      <div className="min-h-0 flex-1 overflow-y-auto px-4 pt-4 pb-10 md:px-5">
        <div className="flex flex-wrap items-center gap-2">
          <p className="min-w-0 flex-1 text-[13px] text-ink-2">
            {data ? `${plural(data.items.length, "automation")}, ${active} active` : "Automations"}
            {data && !data.process_enabled ? <span className="text-danger"> · this server runs none</span> : null}
          </p>
          {data && admin ? (
            <Button
              size="sm"
              variant={data.kill ? "primary" : "ghost"}
              busy={kill.isPending}
              icon={data.kill ? <CirclePlay className="size-3.5" aria-hidden /> : <CirclePause className="size-3.5" aria-hidden />}
              onClick={() => kill.mutate(!data.kill)}
            >
              {data.kill ? "Resume automations" : "Stop all"}
            </Button>
          ) : null}
        </div>
        {kill.error ? <p className="mt-2 text-[13px] text-danger">{errorText(kill.error)}</p> : null}
        {data?.kill ? (
          <div role="status" className="mt-3 flex gap-2 rounded-card bg-warning-tint p-3 text-[13px] text-ink">
            <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
            <p>The kill switch is on: no automation runs. Changes made meanwhile never start runs, and pending retries were cancelled.</p>
          </div>
        ) : null}

        {list.isLoading ? (
          <Skeleton className="mt-4 h-40" />
        ) : list.error ? (
          <div className="mt-4">
            <ErrorState error={list.error} retry={() => void list.refetch()} />
          </div>
        ) : !data?.items.length ? (
          <EmptyState title="No automations yet">
            An automation runs a governed action type, or tells a webhook endpoint, when a change matches its trigger. Define one in the schema (
            <code className="font-mono">[automations.&lt;name&gt;]</code>, or a proposal with put_automation); a person who covers its principal then enables it.
          </EmptyState>
        ) : (
          <div className="mt-4 min-w-0 overflow-x-auto rounded-card bg-raised shadow-card">
            <table className="stack-table w-full text-[13px]">
              <thead>
                <tr className="border-b border-hairline">
                  <th className={th}>Automation</th>
                  <th className={th}>Status</th>
                  <th className={th}>When</th>
                  <th className={th}>Does</th>
                  <th className={th}>Runs as</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-hairline">
                {data.items.map((a) => (
                  <tr key={a.name} className={cx("hover:bg-sunken", a.name === name && "bg-sunken")}>
                    <td className={td} data-label="Automation">
                      <a href={href(["automate", "automations", a.name])} className="block min-w-0 rounded focus-visible:outline-2 focus-visible:outline-accent">
                        <span className="block font-mono text-[12.5px] font-medium text-ink">{a.name}</span>
                        {a.description ? <span className="block max-w-[40ch] truncate text-xs text-ink-3">{a.description}</span> : null}
                      </a>
                    </td>
                    <td className={`${td} md:whitespace-nowrap`} data-label="Status">
                      <AutomationStatus a={a} />
                    </td>
                    <td className={`${td} text-ink-2`} data-label="When">
                      {triggerText(a)}
                    </td>
                    <td className={`${td} text-ink-2`} data-label="Does">
                      {doesText(a)}
                    </td>
                    <td className={`${td} text-ink-2`} data-label="Runs as">
                      {runsAs(a)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {data ? (
          <p className="mt-3 max-w-[80ch] text-xs leading-relaxed text-ink-3">
            {data.note} A run commits its action, receipt, outbox rows and run row together; webhook effects are delivered at least once, and external effects are not
            reversible.
          </p>
        ) : null}
      </div>
      <Panel
        open={Boolean(name)}
        onClose={close}
        wide
        title={
          <div className="min-w-0">
            <p className="truncate font-mono text-[15px] font-semibold tracking-tight">{name}</p>
            <p className="truncate text-xs text-ink-3">{selected ? selected.principal_id : "automation"}</p>
          </div>
        }
      >
        {selected ? <AutomationDetail key={selected.name} a={selected} /> : name && data ? <p className="pt-4 text-sm text-ink-2">No automation named {name}.</p> : null}
      </Panel>
    </div>
  );
}

type RunFilter = "all" | "succeeded" | "failed" | "skipped";

function AutomationDetail({ a }: { a: AutomationItem }) {
  const qc = useQueryClient();
  const scope = useScope();
  const [filter, setFilter] = useState<RunFilter>("all");
  const [open, setOpen] = useState<string | null>(null);
  const runs = useQuery<AutomationRuns>({
    queryKey: ["automation-runs", a.name, filter, ...scope],
    queryFn: () => call("automation.runs", { name: a.name, limit: 50, ...(filter === "all" ? {} : { state: filter === "failed" ? ["failed", "refused"] : [filter] }) }),
    refetchInterval: 10_000,
  });
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["automations"] });
    void qc.invalidateQueries({ queryKey: ["automation-runs", a.name] });
  };
  const act = useMutation({
    mutationFn: ({ action, input }: { action: string; input?: Record<string, unknown> }) => call(action, input ?? { name: a.name }),
    onSuccess: refresh,
  });
  const busy = (x: string, id?: string) => act.isPending && act.variables?.action === x && (!id || act.variables?.input?.id === id);
  const stale = a.reason?.includes("stale_definition") ? a.reason.replace(/^paused: stale_definition: /, "") : null;

  return (
    <div>
      <dl className="mt-3 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-[13px]">
        <dt className="text-ink-3">Status</dt>
        <dd>
          <AutomationStatus a={a} />
        </dd>
        <dt className="text-ink-3">When</dt>
        <dd className="text-ink-2">{triggerText(a)}</dd>
        <dt className="text-ink-3">Does</dt>
        <dd className="text-ink-2">{doesText(a)}</dd>
        <dt className="text-ink-3">Runs as</dt>
        <dd className="text-ink-2">{runsAs(a)}</dd>
        {a.enabled && a.enabled_at ? (
          <>
            <dt className="text-ink-3">Enabled</dt>
            <dd className="text-ink-2">
              by {a.enabled_by}, {ago(a.enabled_at)}
              {a.expires_at ? <span className="block text-xs text-ink-3">expires {when(a.expires_at)}; a person enables it again then</span> : null}
            </dd>
          </>
        ) : null}
      </dl>
      {stale ? (
        <div role="status" className="mt-3 flex gap-2 rounded-card bg-warning-tint p-3 text-[13px] text-ink">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
          <p>
            Something it reaches changed since it was enabled (<span className="font-mono text-[12.5px]">{stale}</span>). It stays stopped until a person reviews it and
            enables it again.
          </p>
        </div>
      ) : null}

      <div className="mt-4 flex flex-wrap gap-2">
        {a.may_enable ? (
          a.status === "active" ? (
            <Button size="sm" busy={busy("automation.disable")} onClick={() => act.mutate({ action: "automation.disable" })}>
              Disable
            </Button>
          ) : (
            <Button size="sm" variant="primary" busy={busy("automation.enable")} onClick={() => act.mutate({ action: "automation.enable" })}>
              {a.enabled ? "Enable again" : "Enable"}
            </Button>
          )
        ) : (
          <p className="text-[13px] text-ink-3">Enabling needs an approver who covers {runsAs(a)}. Agents never can.</p>
        )}
      </div>
      {act.error ? <p className="mt-2 text-[13px] text-danger">{errorText(act.error)}</p> : null}

      <DryRun name={a.name} />

      <Section title="Runs" aside={<Segmented<RunFilter> label="Show runs" value={filter} onChange={setFilter} options={[
        { value: "all", label: "All" },
        { value: "succeeded", label: "Succeeded" },
        { value: "failed", label: "Failed" },
        { value: "skipped", label: "Skipped" },
      ]} />}>
        {runs.isLoading ? (
          <Skeleton className="h-24" />
        ) : runs.error ? (
          <ErrorState error={runs.error} retry={() => void runs.refetch()} />
        ) : !runs.data?.items.length ? (
          <p className="text-[13px] text-ink-3">
            {runs.data?.hidden ? `${plural(runs.data.hidden, "run")} hidden: you do not cover its principal.` : "No runs yet. It runs on changes after it was enabled."}
          </p>
        ) : (
          <ul className="divide-y divide-hairline rounded-card bg-raised shadow-card">
            {runs.data.items.map((r) => (
              <li key={r.id}>
                <button
                  className="flex w-full items-start gap-3 px-3 py-2 text-left hover:bg-sunken focus-visible:outline-2 focus-visible:outline-accent"
                  aria-expanded={open === r.id}
                  onClick={() => setOpen(open === r.id ? null : r.id)}
                >
                  <div className="min-w-0 flex-1">
                    <p className="truncate font-mono text-[12.5px] text-ink">
                      {r.trigger.op} {r.trigger.id}
                    </p>
                    <p className="truncate text-xs text-ink-3">
                      {ago(r.started_at)} · depth {r.depth}
                      {r.attempts > 1 ? ` · ${plural(r.attempts, "attempt")}` : ""}
                      {r.reason ? ` · ${r.reason}` : ""}
                    </p>
                  </div>
                  <Status tone={RUN_TONE[r.state]}>{RUN_LABEL[r.state]}</Status>
                </button>
                {open === r.id ? (
                  <RunDetail
                    run={r}
                    canRerun={a.may_enable && a.status === "active" && (r.state === "skipped" || r.state === "failed")}
                    busy={busy("automation.rerun", r.id)}
                    onRerun={() => act.mutate({ action: "automation.rerun", input: { id: r.id } })}
                  />
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </Section>
    </div>
  );
}

function RunDetail({ run, canRerun, busy, onRerun }: { run: AutomationRun; canRerun: boolean; busy: boolean; onRerun: () => void }) {
  return (
    <div className="px-3 pb-3">
      <p className="font-mono text-[11.5px] text-ink-3">
        {run.id} · change {run.trigger.cursor} by {run.trigger.actor}
      </p>
      {run.effects.length ? (
        <ol className="mt-1.5 flex flex-col gap-1.5">
          {run.effects.map((e) => (
            <li key={e.index} className="rounded-input bg-sunken px-2.5 py-1.5 text-xs">
              <p className="text-ink-2">
                {e.kind === "action" ? "Action" : "Webhook"} <span className="font-mono text-ink">{e.target}</span>
                {e.receipt ? <span className="font-mono text-ink-3"> · receipt {e.receipt}</span> : null}
                {e.outbox ? <span className="font-mono text-ink-3"> · event {e.outbox}</span> : null}
              </p>
              {e.error ? <p className="text-danger">{e.error}</p> : null}
              {e.plan ? <pre className="mt-1 max-h-32 overflow-auto font-mono text-[11.5px] whitespace-pre-wrap text-ink-3">{JSON.stringify(e.plan, null, 2)}</pre> : null}
            </li>
          ))}
        </ol>
      ) : null}
      {canRerun ? (
        <Button className="mt-2" size="sm" variant="ghost" busy={busy} icon={<RotateCcw className="size-3.5" aria-hidden />} onClick={onRerun}>
          Run again
        </Button>
      ) : null}
    </div>
  );
}

interface DryRunResult {
  matched: boolean;
  why_not?: string;
  verdict: string;
  reasons?: string[];
  effects?: { kind: string; target: string; plan?: Record<string, unknown> }[];
  depth: number;
  depth_limit: number;
}

const VERDICT: Record<string, string> = {
  would_run: "Would run",
  "would_run (shadow)": "Would run (shadow: records only)",
  conditions_not_met: "Conditions not met",
  plan_invalid: "The plan is not valid",
  needs_confirmation: "Refused: needs a person's confirmation",
  error: "Cannot render",
};

function DryRun({ name }: { name: string }) {
  const [target, setTarget] = useState("");
  const test = useMutation({
    mutationFn: () => {
      const t = target.trim();
      return call<DryRunResult>("automation.test", /^\d+\.\d+$/.test(t) ? { name, cursor: t } : { name, id: t });
    },
  });
  const r = test.data;
  return (
    <Section title="Dry run">
      <form
        className="flex flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (target.trim()) test.mutate();
        }}
      >
        <div className="min-w-[16rem] flex-1">
          <Field label="Change or entity" hint="A change cursor (42.0), or an entity id as if it was just updated. Nothing is written.">
            {(id) => <Input id={id} value={target} onChange={(e) => setTarget(e.target.value)} placeholder="issue:1 or 42.0" spellCheck={false} autoComplete="off" />}
          </Field>
        </div>
        <Button size="sm" type="submit" busy={test.isPending} disabled={!target.trim()} icon={<FlaskConical className="size-3.5" aria-hidden />}>
          Test
        </Button>
      </form>
      {test.error ? <p className="mt-2 text-[13px] text-danger">{errorText(test.error)}</p> : null}
      {r ? (
        <div className="mt-3 flex flex-col gap-2 text-[13px]">
          <p className="text-ink">
            <Status tone={r.verdict.startsWith("would_run") ? "ok" : "warn"}>{VERDICT[r.verdict] ?? r.verdict}</Status>
            <span className="ml-2 text-ink-3">
              {r.matched ? "the trigger admits it" : r.why_not ?? "the trigger does not admit it"} · depth {r.depth} of {r.depth_limit}
            </span>
          </p>
          {r.reasons?.length ? (
            <ul className="list-disc pl-5 text-ink-2">
              {r.reasons.map((x) => (
                <li key={x}>{x}</li>
              ))}
            </ul>
          ) : null}
          {r.effects?.map((e, i) => (
            <div key={i}>
              <p className="text-xs text-ink-3">
                <Badge>{e.kind}</Badge> <span className="font-mono">{e.target}</span>
              </p>
              {e.plan ? <CodeBlock className="mt-1" code={JSON.stringify(e.plan, null, 2)} label={`${e.kind} ${e.target}`} /> : null}
            </div>
          ))}
        </div>
      ) : null}
    </Section>
  );
}
