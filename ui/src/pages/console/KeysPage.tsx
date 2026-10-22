import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { KeyRound, RotateCw, Trash2 } from "lucide-react";

import { ConfirmDialog, Page, RoleButton, SecretReveal, Status, td, th } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { Button, Checkbox, Dialog, EmptyState, ErrorState, Field, IconButton, Input, Select, Skeleton, TagInput, cx } from "../../components/ui";
import { errorText } from "../../lib/api";
import { atLeast, consoleApi, needs, qk, useConsoleScope, type ApiKey, type KeyScope, type NewKey, type OrgRole } from "../../lib/console";
import { ago, when } from "../../lib/format";

const SCOPES: { value: KeyScope; label: string; hint: string }[] = [
  { value: "read", label: "read", hint: "Questions, search, reads (viewer)" },
  { value: "write", label: "write", hint: "Writes and governed actions (member)" },
  { value: "agent", label: "agent", hint: "For MCP and automation: proposes, never applies schema (agent)" },
  { value: "admin", label: "admin", hint: "Everything in the project, traces included (admin)" },
];

/** The role each scope grants: impersonation may use only these (section 6). */
const SCOPE_ROLE: Record<KeyScope, string> = { read: "viewer", write: "member", agent: "agent", admin: "admin" };

const EXPIRY = [
  { value: "", label: "Never" },
  { value: "30", label: "In 30 days" },
  { value: "90", label: "In 90 days" },
  { value: "365", label: "In a year" },
];

const GRACE = [
  { value: 0, label: "Revoke the old key now" },
  { value: 3600, label: "Keep the old key for 1 hour" },
  { value: 86400, label: "Keep the old key for 24 hours" },
];

function keyStatus(k: ApiKey): { tone: "ok" | "error" | "warn" | "muted"; text: string } {
  const now = Date.now();
  if (k.revoked_at) return { tone: "muted", text: "Revoked" };
  if (k.expires_at && k.expires_at <= now) return { tone: "muted", text: "Expired" };
  if (k.expires_at && k.expires_at - now < 7 * 864e5) return { tone: "warn", text: `Expires ${ago(k.expires_at)}` };
  return { tone: "ok", text: "Active" };
}

/** Developers manage only their own keys without admin scope or impersonation (section 5). */
function canManage(k: ApiKey, role: OrgRole | undefined, me: string | undefined): boolean {
  if (atLeast(role, "admin")) return true;
  return atLeast(role, "developer") && k.created_by === me && !k.impersonate && !k.scopes.includes("admin");
}

function CreateKey({ open, onClose, projectRef, role }: { open: boolean; onClose: () => void; projectRef: string; role?: OrgRole }) {
  const qc = useQueryClient();
  const admin = atLeast(role, "admin");
  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<KeyScope[]>(["read"]);
  const [workspaces, setWorkspaces] = useState<string[]>(["*"]);
  const [expiry, setExpiry] = useState("");
  const [impersonate, setImpersonate] = useState(false);
  const [roles, setRoles] = useState<string[]>([]);
  const [secret, setSecret] = useState<string>();
  const granted = SCOPES.filter((s) => scopes.includes(s.value)).map((s) => SCOPE_ROLE[s.value]);
  useEffect(() => {
    if (open) {
      setName("");
      setScopes(["read"]);
      setWorkspaces(["*"]);
      setExpiry("");
      setImpersonate(false);
      setRoles([]);
      setSecret(undefined);
    }
  }, [open]);
  const create = useMutation({
    mutationFn: () => {
      const k: NewKey = { name: name.trim(), scopes, workspaces: workspaces.length ? workspaces : ["*"] };
      if (expiry) k.expires_at = Date.now() + Number(expiry) * 864e5;
      if (impersonate) {
        k.impersonate = true;
        k.impersonate_roles = roles;
      }
      return consoleApi.createKey(projectRef, k);
    },
    onSuccess: (r) => {
      setSecret(r.secret);
      void qc.invalidateQueries({ queryKey: qk.keys(projectRef) });
      void qc.invalidateQueries({ queryKey: qk.connect(projectRef) });
    },
  });
  const valid = name.trim() && scopes.length && (!impersonate || roles.length);
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={secret ? "Your new API key" : "Create API key"}
      footer={
        secret ? (
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        ) : (
          <>
            <Button onClick={onClose}>Cancel</Button>
            <Button variant="primary" type="submit" form="create-key" busy={create.isPending} disabled={!valid}>
              Create key
            </Button>
          </>
        )
      }
    >
      {secret ? (
        <SecretReveal secret={secret} />
      ) : (
        <form id="create-key" className="flex flex-col gap-4" onSubmit={(e) => (e.preventDefault(), valid && create.mutate())}>
          <Field label="Name" hint="Where it is used, for example checkout-service">
            {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} maxLength={100} required autoFocus />}
          </Field>
          <fieldset className="flex flex-col gap-2">
            <legend className="mb-1.5 text-[13px] font-medium">Scopes</legend>
            {SCOPES.map((s) => {
              const locked = s.value === "admin" && !admin;
              return (
                <div key={s.value} title={locked ? needs("admin") : undefined}>
                  <Checkbox
                    label={s.label}
                    checked={scopes.includes(s.value)}
                    disabled={locked}
                    onChange={(on) => setScopes((cur) => (on ? [...cur, s.value] : cur.filter((x) => x !== s.value)))}
                  />
                  <p className="pl-6 text-xs text-ink-3">{locked ? `${s.hint}. ${needs("admin")}.` : s.hint}</p>
                </div>
              );
            })}
          </fieldset>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="Workspaces" hint="* for every workspace">
              {() => <TagInput value={workspaces} onChange={setWorkspaces} placeholder="main" />}
            </Field>
            <Field label="Expires">
              {(id) => (
                <Select id={id} value={expiry} onChange={(e) => setExpiry(e.target.value)}>
                  {EXPIRY.map((x) => (
                    <option key={x.value} value={x.value}>
                      {x.label}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
          </div>
          <div title={admin ? undefined : needs("admin")}>
            <Checkbox label="May act for your users (impersonation)" checked={impersonate} disabled={!admin} onChange={setImpersonate} />
            <p className="pl-6 text-xs text-ink-3">
              The key sends x-onto-act-as to act for one of your users, with at most the roles below: roles its scopes grant ({granted.join(", ") || "none yet"}).
              {admin ? "" : ` ${needs("admin")}.`}
            </p>
            {impersonate ? (
              <div className="mt-2 pl-6">
                <TagInput value={roles} onChange={setRoles} placeholder={granted.join(", ")} suggestions={granted} />
              </div>
            ) : null}
          </div>
          {create.error ? <p className="text-[13px] text-danger">{errorText(create.error)}</p> : null}
        </form>
      )}
    </Dialog>
  );
}

function RotateKey({ k, onClose, projectRef }: { k?: ApiKey; onClose: () => void; projectRef: string }) {
  const qc = useQueryClient();
  const [grace, setGrace] = useState(0);
  const [secret, setSecret] = useState<string>();
  useEffect(() => {
    setGrace(0);
    setSecret(undefined);
  }, [k?.id]);
  const rotate = useMutation({
    mutationFn: () => consoleApi.rotateKey(projectRef, k!.id, grace),
    onSuccess: (r) => {
      setSecret(r.secret);
      void qc.invalidateQueries({ queryKey: qk.keys(projectRef) });
      void qc.invalidateQueries({ queryKey: qk.connect(projectRef) });
    },
  });
  return (
    <Dialog
      open={Boolean(k)}
      onClose={onClose}
      title={secret ? "The rotated key" : `Rotate ${k?.name ?? ""}`}
      footer={
        secret ? (
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        ) : (
          <>
            <Button onClick={onClose}>Cancel</Button>
            <Button variant="primary" busy={rotate.isPending} onClick={() => rotate.mutate()}>
              Rotate key
            </Button>
          </>
        )
      }
    >
      {secret ? (
        <SecretReveal secret={secret} note={grace ? "The old key keeps working until its grace period ends." : "The old key no longer works."} />
      ) : (
        <div className="flex flex-col gap-3 text-sm text-ink-2">
          <p>A new key with the same name, scopes and workspaces replaces {k?.prefix}. Deploy the new secret, then let the old one lapse.</p>
          <Field label="Old key">
            {(id) => (
              <Select id={id} value={grace} onChange={(e) => setGrace(Number(e.target.value))}>
                {GRACE.map((g) => (
                  <option key={g.value} value={g.value}>
                    {g.label}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          {rotate.error ? <p className="text-danger">{errorText(rotate.error)}</p> : null}
        </div>
      )}
    </Dialog>
  );
}

export default function KeysPage() {
  const qc = useQueryClient();
  const { me, project, role } = useConsoleScope();
  const ref = project?.ref ?? "";
  const keys = useQuery({ queryKey: qk.keys(ref), queryFn: () => consoleApi.keys(ref), enabled: Boolean(project) });
  const members = useQuery({ queryKey: qk.members(project?.org_id ?? ""), queryFn: () => consoleApi.members(project!.org_id), enabled: Boolean(project) });
  const [creating, setCreating] = useState(false);
  const [rotating, setRotating] = useState<ApiKey>();
  const [revoking, setRevoking] = useState<ApiKey>();
  const [showOld, setShowOld] = useState(false);
  const revoke = useMutation({
    mutationFn: (k: ApiKey) => consoleApi.revokeKey(ref, k.id),
    onSuccess: () => {
      setRevoking(undefined);
      void qc.invalidateQueries({ queryKey: qk.keys(ref) });
      void qc.invalidateQueries({ queryKey: qk.connect(ref) });
    },
  });
  const name = (id: string) => members.data?.members.find((m) => m.user_id === id)?.name ?? id;
  const all = keys.data?.keys ?? [];
  const list = showOld ? all : all.filter((k) => keyStatus(k).tone !== "muted");
  const hidden = all.length - list.length;

  return (
    <>
      <PageHeader title="API keys" context="Secret keys for services, scripts and MCP clients; each reaches this project only">
        <RoleButton size="sm" variant="primary" role={role} min="developer" icon={<KeyRound className="size-4" aria-hidden />} onClick={() => setCreating(true)}>
          Create key
        </RoleButton>
      </PageHeader>
      <Page>
        {keys.error ? (
          <ErrorState error={keys.error} retry={() => void keys.refetch()} />
        ) : keys.isLoading ? (
          <Skeleton className="h-40 rounded-card" />
        ) : !all.length ? (
          <EmptyState
            title="No API keys yet"
            action={
              <RoleButton variant="primary" role={role} min="developer" onClick={() => setCreating(true)}>
                Create key
              </RoleButton>
            }
          >
            A key lets a service, script or MCP client call this project. Its secret is shown once.
          </EmptyState>
        ) : (
          <>
            {!list.length ? <p className="py-6 text-center text-[13px] text-ink-3">No active keys. Create one, or show the revoked and expired ones.</p> : null}
            <div className={cx("overflow-x-auto overscroll-x-contain", !list.length && "hidden")}>
              <table className="stack-table w-full border-collapse text-[13px]">
                <thead>
                  <tr className="border-b border-hairline">
                    <th className={th}>Name</th>
                    <th className={th}>Prefix</th>
                    <th className={th}>Scopes</th>
                    <th className={th}>Created</th>
                    <th className={th}>Last used</th>
                    <th className={th}>Expires</th>
                    <th className={th}>Status</th>
                    <th className={th}>
                      <span className="sr-only">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {list.map((k) => {
                    const st = keyStatus(k);
                    const live = st.tone !== "muted";
                    const ok = canManage(k, role, me?.user.id);
                    const why = atLeast(role, "developer") ? "Developers manage only their own keys without admin scope or impersonation" : needs("developer");
                    return (
                      <tr key={k.id} className={cx("border-b border-hairline", !live && "text-ink-3")}>
                        <td className={cx(td, "min-w-[140px]")} data-primary>
                          {k.name}
                          {k.impersonate ? <span className="ml-2 text-xs text-ink-3">impersonates</span> : null}
                        </td>
                        <td className={td} data-label="Prefix">
                          <span className="font-mono text-[12.5px]">{k.prefix}</span>
                        </td>
                        <td className={cx(td, "text-ink-2")} data-label="Scopes">
                          {k.scopes.join(", ")}
                          {k.workspaces.join(",") !== "*" ? <span className="block text-xs text-ink-3">{k.workspaces.join(", ")}</span> : null}
                        </td>
                        <td className={cx(td, "text-ink-2")} data-label="Created" title={when(k.created_at)}>
                          <span className="whitespace-nowrap">{ago(k.created_at)}</span>
                          <span className="block text-xs text-ink-3">by {name(k.created_by)}</span>
                        </td>
                        <td className={cx(td, "whitespace-nowrap text-ink-2")} data-label="Last used">
                          {k.last_used_at ? ago(k.last_used_at) : "Never"}
                        </td>
                        <td className={cx(td, "whitespace-nowrap text-ink-2")} data-label="Expires" title={k.expires_at ? when(k.expires_at) : undefined}>
                          {k.expires_at ? ago(k.expires_at) : "Never"}
                        </td>
                        <td className={td} data-label="Status">
                          <Status tone={st.tone}>{st.text}</Status>
                        </td>
                        <td className={cx(td, "text-right")} data-label="">
                          {live ? (
                            <span className="inline-flex gap-0.5" title={ok ? undefined : why}>
                              <IconButton label={`Rotate ${k.name}`} disabled={!ok} onClick={() => setRotating(k)}>
                                <RotateCw className="size-4" aria-hidden />
                              </IconButton>
                              <IconButton label={`Revoke ${k.name}`} disabled={!ok} onClick={() => setRevoking(k)} className="hover:text-danger">
                                <Trash2 className="size-4" aria-hidden />
                              </IconButton>
                            </span>
                          ) : null}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
            {hidden || showOld ? (
              <button onClick={() => setShowOld((v) => !v)} className="mt-3 text-[13px] text-accent hover:underline">
                {showOld ? "Hide revoked and expired keys" : `Show ${hidden} revoked or expired key${hidden === 1 ? "" : "s"}`}
              </button>
            ) : null}
          </>
        )}
      </Page>
      <CreateKey open={creating} onClose={() => setCreating(false)} projectRef={ref} role={role} />
      <RotateKey k={rotating} onClose={() => setRotating(undefined)} projectRef={ref} />
      <ConfirmDialog
        open={Boolean(revoking)}
        onClose={() => setRevoking(undefined)}
        title={`Revoke ${revoking?.name ?? "key"}`}
        confirm="Revoke key"
        busy={revoke.isPending}
        error={revoke.error ? errorText(revoke.error) : undefined}
        onConfirm={() => revoking && revoke.mutate(revoking)}
      >
        <p>
          Requests with <span className="font-mono text-ink">{revoking?.prefix}</span> stop working at once on this server and within 10 seconds everywhere. This cannot be undone.
        </p>
      </ConfirmDialog>
    </>
  );
}
