import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { UserPlus } from "lucide-react";

import { Card, CodeBlock, ConfirmDialog, CopyButton, Page, RoleButton, td, th } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { Button, Dialog, EmptyState, ErrorState, Field, Input, Select, Skeleton, cx } from "../../components/ui";
import { errorText } from "../../lib/api";
import { ROLES, atLeast, consoleApi, needs, qk, useConsoleScope, type Member, type OrgRole } from "../../lib/console";
import { ago, when } from "../../lib/format";
import { navigate, setQuery, useRoute } from "../../lib/router";

const ROLE_HINT: Record<OrgRole, string> = {
  owner: "Everything, including owners and deleting the organization",
  admin: "Members, projects, keys, LLM settings and the audit log",
  developer: "Keys, test connection, scores; writes through the data plane",
  viewer: "Read projects, traces, usage and snippets",
};

/** Links of invitations made in this tab: the server keeps only a hash of the code. */
const links = new Map<string, string>();

function InviteDialog({ open, onClose, org }: { open: boolean; onClose: () => void; org: string }) {
  const qc = useQueryClient();
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<OrgRole>("developer");
  const [link, setLink] = useState<string>();
  useEffect(() => {
    if (open) {
      setEmail("");
      setRole("developer");
      setLink(undefined);
    }
  }, [open]);
  const invite = useMutation({
    mutationFn: () => consoleApi.invite(org, email.trim(), role),
    onSuccess: (r) => {
      links.set(r.invitation.id, r.link);
      setLink(r.link);
      void qc.invalidateQueries({ queryKey: qk.invitations(org) });
    },
  });
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={link ? "Share the invitation" : "Invite a member"}
      footer={
        link ? (
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        ) : (
          <>
            <Button onClick={onClose}>Cancel</Button>
            <Button variant="primary" type="submit" form="invite" busy={invite.isPending} disabled={!email.includes("@")}>
              Create invitation
            </Button>
          </>
        )
      }
    >
      {link ? (
        <div className="flex flex-col gap-3 text-sm text-ink-2">
          <p>This server sends no email. Send this link to {email}; it is valid for 7 days and works once. Signing in with an identity provider accepts it.</p>
          <CodeBlock code={link} label="Invitation link" />
        </div>
      ) : (
        <form id="invite" className="flex flex-col gap-4" onSubmit={(e) => (e.preventDefault(), email.includes("@") && invite.mutate())}>
          <Field label="Email" hint="Members whose verified email matches join on their next sign-in">
            {(id) => <Input id={id} type="email" value={email} onChange={(e) => setEmail(e.target.value)} required autoFocus />}
          </Field>
          <Field label="Role" hint={`${ROLE_HINT[role]}. Owners are not invited: promote a member in the table.`}>
            {(id) => (
              <Select id={id} value={role} onChange={(e) => setRole(e.target.value as OrgRole)}>
                {ROLES.filter((r) => r !== "owner").map((r) => (
                  <option key={r} value={r}>
                    {r}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          {invite.error ? <p className="text-[13px] text-danger">{errorText(invite.error)}</p> : null}
        </form>
      )}
    </Dialog>
  );
}

export default function MembersPage() {
  const route = useRoute();
  const qc = useQueryClient();
  const { me, org: current } = useConsoleScope();
  const org = me?.orgs.find((o) => o.id === route.query.get("org")) ?? current;
  const id = org?.id ?? "";
  const myRole = org?.role;
  const admin = atLeast(myRole, "admin");
  const members = useQuery({ queryKey: qk.members(id), queryFn: () => consoleApi.members(id), enabled: Boolean(org) });
  const invitations = useQuery({ queryKey: qk.invitations(id), queryFn: () => consoleApi.invitations(id), enabled: Boolean(org) && admin });
  const [inviting, setInviting] = useState(false);
  const [removing, setRemoving] = useState<Member>();
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: qk.members(id) });
    void qc.invalidateQueries({ queryKey: ["console", "me"] });
  };
  const setRole = useMutation({ mutationFn: (v: { user: string; role: OrgRole }) => consoleApi.setRole(id, v.user, v.role), onSettled: refresh });
  const remove = useMutation({
    mutationFn: (m: Member) => consoleApi.removeMember(id, m.user_id),
    onSuccess: (_, m) => {
      setRemoving(undefined);
      refresh();
      if (m.user_id === me?.user.id) navigate(["projects"]);
    },
  });
  const revoke = useMutation({ mutationFn: (inv: string) => consoleApi.revokeInvitation(id, inv), onSuccess: () => qc.invalidateQueries({ queryKey: qk.invitations(id) }) });
  const pending = (invitations.data?.invitations ?? []).filter((i) => !i.accepted_at && i.expires_at > Date.now());

  return (
    <>
      <PageHeader title="Members" context={org ? `${org.name} · you are ${myRole}` : undefined}>
        {me && me.orgs.length > 1 ? (
          <Select aria-label="Organization" value={id} onChange={(e) => setQuery({ org: e.target.value })} className="h-8 max-w-44 text-[13px]">
            {me.orgs.map((o) => (
              <option key={o.id} value={o.id}>
                {o.name}
              </option>
            ))}
          </Select>
        ) : null}
        <RoleButton size="sm" variant="primary" role={myRole} min="admin" icon={<UserPlus className="size-4" aria-hidden />} onClick={() => setInviting(true)}>
          Invite
        </RoleButton>
      </PageHeader>
      <Page>
        {!org ? (
          <EmptyState title="No organization">You are not a member of an organization.</EmptyState>
        ) : (
          <div className="flex flex-col gap-4">
            {members.error ? <ErrorState error={members.error} retry={() => void members.refetch()} /> : null}
            {setRole.error ? <p className="text-[13px] text-danger">{errorText(setRole.error)}</p> : null}
            <Card title="Members" pad={false}>
              {members.isLoading ? (
                <Skeleton className="m-4 h-24" />
              ) : (
                <div className="px-4 pb-2">
                  <div className="overflow-x-auto overscroll-x-contain">
                    <table className="stack-table w-full border-collapse text-[13px]">
                      <thead>
                        <tr className="border-b border-hairline">
                          <th className={th}>Name</th>
                          <th className={th}>Email</th>
                          <th className={th}>Role</th>
                          <th className={th}>Joined</th>
                          <th className={th}>
                            <span className="sr-only">Actions</span>
                          </th>
                        </tr>
                      </thead>
                      <tbody>
                        {(members.data?.members ?? []).map((m) => {
                          const self = m.user_id === me?.user.id;
                          const ownerRow = m.role === "owner";
                          const canEdit = admin && (!ownerRow || myRole === "owner");
                          const canRemove = self || canEdit;
                          return (
                            <tr key={m.user_id} className="border-b border-hairline last:border-0">
                              <td className={td} data-primary>
                                {m.name || m.user_id}
                                {self ? <span className="ml-1.5 text-xs text-ink-3">you</span> : null}
                              </td>
                              <td className={cx(td, "text-ink-2")} data-label="Email">
                                {m.email ?? "–"}
                              </td>
                              <td className={td} data-label="Role">
                                <span title={canEdit ? undefined : ownerRow && admin ? "Only owners change owners" : needs("admin")} className="inline-flex">
                                  <Select
                                    aria-label={`Role of ${m.name || m.user_id}`}
                                    value={m.role}
                                    disabled={!canEdit || setRole.isPending}
                                    onChange={(e) => setRole.mutate({ user: m.user_id, role: e.target.value as OrgRole })}
                                    className="h-8 max-w-32 text-[13px]"
                                  >
                                    {ROLES.filter((r) => r !== "owner" || myRole === "owner" || m.role === "owner").map((r) => (
                                      <option key={r} value={r}>
                                        {r}
                                      </option>
                                    ))}
                                  </Select>
                                </span>
                              </td>
                              <td className={cx(td, "whitespace-nowrap text-ink-2")} data-label="Joined" title={when(m.joined_at)}>
                                {ago(m.joined_at)}
                              </td>
                              <td className={cx(td, "text-right")} data-label="">
                                <span title={canRemove ? undefined : needs(ownerRow ? "owner" : "admin")} className="inline-flex">
                                  <Button size="sm" variant="ghost" disabled={!canRemove} onClick={() => setRemoving(m)} className="hover:text-danger">
                                    {self ? "Leave" : "Remove"}
                                  </Button>
                                </span>
                              </td>
                            </tr>
                          );
                        })}
                      </tbody>
                    </table>
                  </div>
                </div>
              )}
            </Card>
            {admin ? (
              <Card title="Pending invitations" pad={false}>
                {invitations.error ? (
                  <div className="p-4">
                    <ErrorState error={invitations.error} />
                  </div>
                ) : !pending.length ? (
                  <p className="px-4 pb-4 text-[13px] text-ink-3">No pending invitations.</p>
                ) : (
                  <div className="px-4 pb-2">
                    <div className="overflow-x-auto overscroll-x-contain">
                      <table className="stack-table w-full border-collapse text-[13px]">
                        <thead>
                          <tr className="border-b border-hairline">
                            <th className={th}>Email</th>
                            <th className={th}>Role</th>
                            <th className={th}>Expires</th>
                            <th className={th}>Link</th>
                            <th className={th}>
                              <span className="sr-only">Actions</span>
                            </th>
                          </tr>
                        </thead>
                        <tbody>
                          {pending.map((i) => (
                            <tr key={i.id} className="border-b border-hairline last:border-0">
                              <td className={td} data-primary>
                                {i.email}
                              </td>
                              <td className={cx(td, "text-ink-2")} data-label="Role">
                                {i.role}
                              </td>
                              <td className={cx(td, "whitespace-nowrap text-ink-2")} data-label="Expires" title={when(i.expires_at)}>
                                {ago(i.expires_at)}
                              </td>
                              <td className={td} data-label="Link">
                                {links.get(i.id) ? (
                                  <CopyButton text={links.get(i.id)!} label="Copy link" />
                                ) : (
                                  <span className="text-xs text-ink-3" title="The server keeps only a hash of the code; revoke and invite again for a new link">
                                    Shown when created
                                  </span>
                                )}
                              </td>
                              <td className={cx(td, "text-right")} data-label="">
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  busy={revoke.isPending && revoke.variables === i.id}
                                  onClick={() => revoke.mutate(i.id)}
                                  className="hover:text-danger"
                                >
                                  Revoke
                                </Button>
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  </div>
                )}
              </Card>
            ) : null}
          </div>
        )}
      </Page>
      <InviteDialog open={inviting} onClose={() => setInviting(false)} org={id} />
      <ConfirmDialog
        open={Boolean(removing)}
        onClose={() => setRemoving(undefined)}
        title={removing?.user_id === me?.user.id ? `Leave ${org?.name}` : `Remove ${removing?.name || removing?.user_id}`}
        confirm={removing?.user_id === me?.user.id ? "Leave organization" : "Remove member"}
        busy={remove.isPending}
        error={remove.error ? errorText(remove.error) : undefined}
        onConfirm={() => removing && remove.mutate(removing)}
      >
        <p>
          {removing?.user_id === me?.user.id
            ? "You lose access to its projects at once."
            : "They lose access to this organization's projects within 30 seconds. Their API keys keep working until revoked."}
        </p>
      </ConfirmDialog>
    </>
  );
}
