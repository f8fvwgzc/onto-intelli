// Webhook endpoints: who receives what, as which principal, and what was
// delivered. Admins manage endpoints whose principal they cover; the server
// refuses everyone else and shows them only an endpoint's origin.

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CirclePause, CirclePlay, KeyRound, Plus, RotateCcw, Send, Trash2, TriangleAlert } from "lucide-react";

import { CodeBlock, ConfirmDialog, Status, td, th } from "../../components/consoleUi";
import { Badge, Button, Checkbox, Dialog, EmptyState, ErrorState, Field, Input, Panel, Section, Skeleton, TagInput, cx } from "../../components/ui";
import { call, errorText } from "../../lib/api";
import { ago, plural, when } from "../../lib/format";
import { useSchema, useScope, useWhoami } from "../../lib/queries";
import { href, navigate } from "../../lib/router";
import type { DeliveryAttempt, OutboxRow, OutboxState, WebhookEndpoint, WebhookList } from "../../lib/types";

const OPS = ["entity.create", "entity.update", "entity.delete", "entity.hidden", "link.create", "link.update", "link.delete", "schema.change"];

const STATE_TONE: Record<OutboxState, "ok" | "error" | "warn" | "muted"> = {
  pending: "muted",
  inflight: "muted",
  retry: "warn",
  delivered: "ok",
  dead: "error",
  cancelled: "muted",
};

function sees(ep: WebhookEndpoint): string {
  const ws = ep.principal.workspaces.includes("*") ? "every workspace" : ep.principal.workspaces.join(", ");
  return `${ep.principal.roles.join(" + ")} in ${ws}`;
}

function subscription(ep: WebhookEndpoint): string {
  const f = ep.filter;
  if (!f) return "Effects only";
  const parts: string[] = [];
  if (f.kinds?.length) parts.push(f.kinds.join(", "));
  if (f.ops?.length) parts.push(f.ops.join(", "));
  if (f.rels?.length) parts.push(`via ${f.rels.join(", ")}`);
  return parts.length ? parts.join(" · ") : "Every change";
}

function EndpointStatus({ ep }: { ep: WebhookEndpoint }) {
  if (ep.status === "disabled") return <Status tone="muted">Disabled</Status>;
  if (ep.status === "paused") {
    const why = ep.paused?.reason === "backlogged" ? "backlogged" : ep.paused?.reason === "failing" ? "failing" : "paused";
    return <Status tone="warn">Paused, {why}</Status>;
  }
  return <Status tone="ok">Active</Status>;
}

export function WebhooksTab({ name }: { name?: string }) {
  const scope = useScope();
  const qc = useQueryClient();
  const me = useWhoami().data;
  const admin = me?.capabilities.configure_outbound ?? false;
  const [adding, setAdding] = useState(false);
  const list = useQuery<WebhookList>({
    queryKey: ["webhooks", ...scope],
    queryFn: () => call<WebhookList>("webhook.list"),
    enabled: admin,
    refetchInterval: 15_000,
  });
  const kill = useMutation({
    mutationFn: (on: boolean) => call("webhook.kill", on ? { on: true } : { off: true }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["webhooks"] }),
  });
  const close = () => navigate(["automate", "webhooks"]);
  const data = list.data;
  const selected = data?.items.find((e) => e.name === name);

  if (me && !admin) {
    return (
      <EmptyState title="Webhooks are configured by admins">
        Endpoints send what their principal may read to systems outside this project, so only admins who cover that principal may add, change or read them. Agents never
        can.
      </EmptyState>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col md:flex-row">
      <div className="min-h-0 flex-1 overflow-y-auto px-4 pt-4 pb-10 md:px-5">
        <div className="flex flex-wrap items-center gap-2">
          <p className="min-w-0 flex-1 text-[13px] text-ink-2">
            {data ? `${plural(data.items.length, "endpoint")} of ${data.max_endpoints}` : "Endpoints"}
            {data?.kill ? <span className="text-danger"> · deliveries are held</span> : null}
          </p>
          {data ? (
            <Button
              size="sm"
              variant={data.kill ? "primary" : "ghost"}
              busy={kill.isPending}
              icon={data.kill ? <CirclePlay className="size-3.5" aria-hidden /> : <CirclePause className="size-3.5" aria-hidden />}
              onClick={() => kill.mutate(!data.kill)}
            >
              {data.kill ? "Resume deliveries" : "Hold deliveries"}
            </Button>
          ) : null}
          <Button size="sm" variant="primary" icon={<Plus className="size-3.5" aria-hidden />} onClick={() => setAdding(true)}>
            Add endpoint
          </Button>
        </div>
        {kill.error ? <p className="mt-2 text-[13px] text-danger">{errorText(kill.error)}</p> : null}
        {data?.kill ? (
          <div role="status" className="mt-3 flex gap-2 rounded-card bg-warning-tint p-3 text-[13px] text-ink">
            <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
            <p>The kill switch is on: nothing is sent. Rows wait in the outbox and subscriptions keep their cursors, so nothing is lost when deliveries resume.</p>
          </div>
        ) : null}

        {list.isLoading ? (
          <Skeleton className="mt-4 h-40" />
        ) : list.error ? (
          <div className="mt-4">
            <ErrorState error={list.error} retry={() => void list.refetch()} />
          </div>
        ) : !data?.items.length ? (
          <EmptyState title="No endpoints yet" action={<Button onClick={() => setAdding(true)}>Add an endpoint</Button>}>
            An endpoint receives signed CloudEvents for the changes it subscribes to, rendered as its principal sees them.
          </EmptyState>
        ) : (
          <div className="mt-4 min-w-0 overflow-x-auto rounded-card bg-raised shadow-card">
            <table className="stack-table w-full text-[13px]">
              <thead>
                <tr className="border-b border-hairline">
                  <th className={th}>Endpoint</th>
                  <th className={th}>Status</th>
                  <th className={th}>Sees</th>
                  <th className={th}>Subscribes to</th>
                  <th className={`${th} text-right`}>Pending</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-hairline">
                {data.items.map((ep) => (
                  <tr key={ep.name} className={cx("hover:bg-sunken", ep.name === name && "bg-sunken")}>
                    <td className={td} data-label="Endpoint">
                      <a href={href(["automate", "webhooks", ep.name])} className="block min-w-0 rounded focus-visible:outline-2 focus-visible:outline-accent">
                        <span className="block font-mono text-[12.5px] font-medium text-ink">{ep.name}</span>
                        <span className="block max-w-[38ch] truncate font-mono text-xs text-ink-3" title={ep.url}>
                          {ep.url}
                        </span>
                      </a>
                    </td>
                    <td className={`${td} md:whitespace-nowrap`} data-label="Status">
                      <EndpointStatus ep={ep} />
                    </td>
                    <td className={`${td} text-ink-2`} data-label="Sees">
                      {sees(ep)}
                    </td>
                    <td className={`${td} text-ink-2`} data-label="Subscribes to">
                      {subscription(ep)}
                    </td>
                    <td className={`${td} tnum text-right text-ink-2`} data-label="Pending">
                      {ep.may_manage ? (ep.pending ?? 0).toLocaleString() : "–"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {data ? <p className="mt-3 max-w-[80ch] text-xs leading-relaxed text-ink-3">{data.note} Signatures follow Standard Webhooks; the envelope is CloudEvents 1.0.</p> : null}
      </div>
      <Panel
        open={Boolean(name)}
        onClose={close}
        wide
        title={
          <div className="min-w-0">
            <p className="truncate font-mono text-[15px] font-semibold tracking-tight">{name}</p>
            <p className="truncate text-xs text-ink-3">{selected ? `${selected.principal_id} · version ${selected.version}` : "webhook endpoint"}</p>
          </div>
        }
      >
        {selected ? <EndpointDetail key={selected.name} ep={selected} onRemoved={close} /> : name && data ? <p className="pt-4 text-sm text-ink-2">No endpoint named {name}.</p> : null}
      </Panel>
      <AddDialog open={adding} onClose={() => setAdding(false)} />
    </div>
  );
}

function EndpointDetail({ ep, onRemoved }: { ep: WebhookEndpoint; onRemoved: () => void }) {
  const qc = useQueryClient();
  const scope = useScope();
  const [removing, setRemoving] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const deliveries = useQuery<{ items: OutboxRow[] }>({
    queryKey: ["webhook-deliveries", ep.name, ...scope],
    queryFn: () => call("webhook.deliveries", { name: ep.name, limit: 50 }),
    enabled: ep.may_manage,
    refetchInterval: 10_000,
  });
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["webhooks"] });
    void qc.invalidateQueries({ queryKey: ["webhook-deliveries", ep.name] });
  };
  const act = useMutation({
    mutationFn: ({ action, input }: { action: string; input?: Record<string, unknown> }) => call<{ secret?: string }>(action, { name: ep.name, ...input }),
    onSuccess: (r, v) => {
      if (v.action === "webhook.remove") onRemoved();
      if (r?.secret) setSecret(r.secret);
      refresh();
    },
  });
  const busy = (a: string) => act.isPending && act.variables?.action === a;
  const dead = (deliveries.data?.items ?? []).filter((r) => r.state === "dead").map((r) => r.id);

  if (!ep.may_manage) {
    return (
      <p className="pt-4 text-sm text-ink-2">
        This endpoint acts as {sees(ep)}, which your role does not cover. Ask an admin who does to change it or read its deliveries.
      </p>
    );
  }

  return (
    <div>
      <dl className="mt-3 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-[13px]">
        <dt className="text-ink-3">URL</dt>
        <dd className="min-w-0 font-mono text-[12.5px] break-all text-ink">{ep.url}</dd>
        <dt className="text-ink-3">Status</dt>
        <dd>
          <EndpointStatus ep={ep} />
        </dd>
        <dt className="text-ink-3">Sees</dt>
        <dd className="text-ink-2">{sees(ep)}</dd>
        <dt className="text-ink-3">Subscribes to</dt>
        <dd className="text-ink-2">
          {subscription(ep)}
          {ep.cursor ? <span className="block font-mono text-xs text-ink-3">cursor {ep.cursor}</span> : null}
        </dd>
        <dt className="text-ink-3">Secret</dt>
        <dd className="font-mono text-[12.5px] text-ink-2">
          {ep.secret}
          {ep.previous_until ? <span className="block font-sans text-xs text-ink-3">previous secret also signs until {when(ep.previous_until)}</span> : null}
        </dd>
        {ep.restricted ? (
          <>
            <dt className="text-ink-3">Reach</dt>
            <dd className="text-ink-2">Public addresses only (registered with a project key)</dd>
          </>
        ) : null}
      </dl>

      <div className="mt-4 flex flex-wrap gap-2">
        <Button size="sm" icon={<Send className="size-3.5" aria-hidden />} busy={busy("webhook.test")} onClick={() => act.mutate({ action: "webhook.test" })}>
          Send test
        </Button>
        {ep.status === "paused" ? (
          <Button size="sm" icon={<CirclePlay className="size-3.5" aria-hidden />} busy={busy("webhook.resume")} onClick={() => act.mutate({ action: "webhook.resume" })}>
            Resume
          </Button>
        ) : null}
        <Button
          size="sm"
          busy={busy("webhook.update")}
          onClick={() => act.mutate({ action: "webhook.update", input: ep.enabled ? { disabled: true } : { enabled: true } })}
        >
          {ep.enabled ? "Disable" : "Enable"}
        </Button>
        <Button size="sm" icon={<KeyRound className="size-3.5" aria-hidden />} busy={busy("webhook.rotate_secret")} onClick={() => act.mutate({ action: "webhook.rotate_secret" })}>
          Rotate secret
        </Button>
        <Button size="sm" variant="danger" icon={<Trash2 className="size-3.5" aria-hidden />} onClick={() => setRemoving(true)}>
          Remove
        </Button>
      </div>
      {act.error ? <p className="mt-2 text-[13px] text-danger">{errorText(act.error)}</p> : null}
      {secret ? (
        <div className="mt-3 flex flex-col gap-2">
          <p className="text-[13px] text-ink-2">The new signing secret is shown once. The previous one keeps signing for a day, so receivers can switch over.</p>
          <CodeBlock code={secret} label="Signing secret" />
        </div>
      ) : null}

      <Section
        title="Deliveries"
        aside={
          dead.length ? (
            <Button
              size="sm"
              variant="ghost"
              icon={<RotateCcw className="size-3.5" aria-hidden />}
              busy={busy("webhook.replay")}
              onClick={() => act.mutate({ action: "webhook.replay", input: { ids: dead } })}
            >
              Replay {plural(dead.length, "dead row")}
            </Button>
          ) : null
        }
      >
        {deliveries.isLoading ? (
          <Skeleton className="h-24" />
        ) : deliveries.error ? (
          <ErrorState error={deliveries.error} retry={() => void deliveries.refetch()} />
        ) : !deliveries.data?.items.length ? (
          <p className="text-[13px] text-ink-3">Nothing sent yet. Send a test, or change something the endpoint subscribes to.</p>
        ) : (
          <ul className="divide-y divide-hairline rounded-card bg-raised shadow-card">
            {deliveries.data.items.map((r) => (
              <li key={r.id}>
                <button
                  className="flex w-full items-start gap-3 px-3 py-2 text-left hover:bg-sunken focus-visible:outline-2 focus-visible:outline-accent"
                  aria-expanded={open === r.id}
                  onClick={() => setOpen(open === r.id ? null : r.id)}
                >
                  <div className="min-w-0 flex-1">
                    <p className="truncate font-mono text-[12.5px] text-ink">{r.event_type}</p>
                    <p className="truncate text-xs text-ink-3">
                      {ago(r.created_at)}
                      {r.cursor ? ` · ${r.cursor}` : ""}
                      {r.attempts > 1 ? ` · ${plural(r.attempts, "attempt")}` : ""}
                      {r.last_error ? ` · ${r.last_error}` : ""}
                    </p>
                  </div>
                  <Status tone={STATE_TONE[r.state]}>
                    {r.state}
                    {r.last_status ? <span className="tnum"> {r.last_status}</span> : null}
                  </Status>
                </button>
                {open === r.id ? <Attempts endpoint={ep.name} id={r.id} /> : null}
              </li>
            ))}
          </ul>
        )}
      </Section>

      <ConfirmDialog
        open={removing}
        onClose={() => setRemoving(false)}
        title={`Remove ${ep.name}`}
        confirm="Remove endpoint"
        type={ep.name}
        busy={busy("webhook.remove")}
        error={act.error && act.variables?.action === "webhook.remove" ? errorText(act.error) : undefined}
        onConfirm={() => act.mutate({ action: "webhook.remove" }, { onSuccess: () => setRemoving(false) })}
      >
        <p>Nothing more is sent to it, and rows still waiting for it are cancelled. Its delivery history stays until retention removes it.</p>
      </ConfirmDialog>
    </div>
  );
}

function Attempts({ endpoint, id }: { endpoint: string; id: string }) {
  const q = useQuery<{ row: OutboxRow; attempts: DeliveryAttempt[] }>({
    queryKey: ["webhook-delivery", endpoint, id],
    queryFn: () => call("webhook.deliveries", { name: endpoint, id }),
  });
  if (q.isLoading) return <Skeleton className="mx-3 mb-2 h-10" />;
  if (q.error) return <p className="px-3 pb-2 text-xs text-danger">{errorText(q.error)}</p>;
  const attempts = q.data?.attempts ?? [];
  return (
    <div className="px-3 pb-3">
      <p className="font-mono text-[11.5px] text-ink-3">id {id}</p>
      {attempts.length ? (
        <ol className="mt-1.5 flex flex-col gap-1.5">
          {attempts.map((a) => (
            <li key={`${a.attempt}-${a.ts}`} className="rounded-input bg-sunken px-2.5 py-1.5 text-xs">
              <p className="tnum text-ink-2">
                Attempt {a.attempt} · {when(a.ts)} · {a.status ? `HTTP ${a.status}` : "no answer"} · {a.duration_ms} ms
              </p>
              {a.error ? <p className="text-ink">{a.error}</p> : null}
              {a.response ? <pre className="mt-1 max-h-24 overflow-auto font-mono text-[11.5px] whitespace-pre-wrap text-ink-3">{a.response}</pre> : null}
            </li>
          ))}
        </ol>
      ) : (
        <p className="mt-1 text-xs text-ink-3">Not attempted yet.</p>
      )}
    </div>
  );
}

function AddDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const qc = useQueryClient();
  const schema = useSchema().data;
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [secret, setSecret] = useState("");
  const [subscribe, setSubscribe] = useState(true);
  const [kinds, setKinds] = useState<string[]>([]);
  const [ops, setOps] = useState<string[]>([]);
  const [roles, setRoles] = useState<string[]>(["viewer"]);
  const [workspaces, setWorkspaces] = useState<string[]>([]);
  const [created, setCreated] = useState<string | null>(null);
  const add = useMutation({
    mutationFn: () =>
      call<{ secret?: string; endpoint: WebhookEndpoint }>("webhook.add", {
        name: name.trim(),
        url: url.trim(),
        ...(secret.trim() ? { secret: secret.trim() } : {}),
        ...(subscribe ? { filter: { kinds, ops } } : {}),
        ...(workspaces.length ? { principal: { roles, workspaces } } : {}),
      }),
    onSuccess: (r) => {
      void qc.invalidateQueries({ queryKey: ["webhooks"] });
      if (r.secret) setCreated(r.secret);
      else finish();
    },
  });
  const reset = () => {
    setName("");
    setUrl("");
    setSecret("");
    setKinds([]);
    setOps([]);
    setRoles(["viewer"]);
    setWorkspaces([]);
    setCreated(null);
    add.reset();
  };
  const finish = () => {
    reset();
    onClose();
  };
  const kindNames = Object.keys(schema?.object_types ?? {});
  const valid = /^[a-z0-9_-]{1,40}$/.test(name.trim()) && url.trim().length > 0 && roles.length > 0;

  if (created) {
    return (
      <Dialog open={open} onClose={finish} title="Endpoint added" footer={<Button variant="primary" onClick={finish}>Done</Button>}>
        <div className="flex flex-col gap-3">
          <div role="alert" className="flex gap-2 rounded-card bg-warning-tint p-3 text-[13px] text-ink">
            <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
            <p>Copy the signing secret now and give it to the receiver. It is shown once; only a sealed copy is kept.</p>
          </div>
          <CodeBlock code={created} label="Signing secret (Standard Webhooks)" />
        </div>
      </Dialog>
    );
  }

  return (
    <Dialog
      open={open}
      onClose={finish}
      title="Add a webhook endpoint"
      footer={
        <>
          <Button onClick={finish}>Cancel</Button>
          <Button variant="primary" busy={add.isPending} disabled={!valid} onClick={() => add.mutate()}>
            Add endpoint
          </Button>
        </>
      }
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) add.mutate();
        }}
      >
        <Field label="Name" hint="Lowercase letters, digits, - and _. The receiver sees it as webhook:<name>.">
          {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} placeholder="ops-alerts" autoComplete="off" spellCheck={false} />}
        </Field>
        <Field label="URL" hint="https only. Private and link-local addresses are refused unless the operator allows them.">
          {(id) => <Input id={id} value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://hooks.example.com/onto" inputMode="url" spellCheck={false} />}
        </Field>
        <Field label="Signing secret" hint="Leave empty to generate one (shown once).">
          {(id) => <Input id={id} type="password" value={secret} onChange={(e) => setSecret(e.target.value)} autoComplete="new-password" />}
        </Field>
        <div className="flex flex-col gap-2">
          <Checkbox label="Subscribe to changes" checked={subscribe} onChange={setSubscribe} />
          {subscribe ? (
            <div className="flex flex-col gap-3 pl-6">
              <Field label="Kinds" hint="Empty: every kind.">
                {() => <TagInput value={kinds} onChange={setKinds} placeholder="issue, project" suggestions={kindNames} />}
              </Field>
              <Field label="Operations" hint="Empty: every operation.">
                {() => <TagInput value={ops} onChange={setOps} placeholder="entity.update" suggestions={OPS} />}
              </Field>
            </div>
          ) : (
            <p className="pl-6 text-xs text-ink-3">Without a subscription it receives only what actions and automations send to it.</p>
          )}
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Receiver sees as" hint="Roles of its principal.">
            {() => <TagInput value={roles} onChange={setRoles} suggestions={["viewer", "member", "admin"]} />}
          </Field>
          <Field label="In workspaces" hint="Empty: this workspace only.">
            {() => <TagInput value={workspaces} onChange={setWorkspaces} placeholder="acme" />}
          </Field>
        </div>
        {add.error ? <p className="text-[13px] text-danger">{errorText(add.error)}</p> : null}
        <p className="text-xs text-ink-3">
          Each change is enqueued once and delivered at least once: receivers dedupe on the event id. <Badge>CloudEvents 1.0</Badge>
        </p>
      </form>
    </Dialog>
  );
}
