import { useEffect, useState } from "react";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { Card, ConfirmDialog, Page, RoleButton, td, th } from "../../components/consoleUi";
import { selectProject } from "../../components/ProjectSwitcher";
import { PageHeader } from "../../components/Shell";
import { Checkbox, ErrorState, Field, Input, Skeleton, cx } from "../../components/ui";
import { errorText } from "../../lib/api";
import { atLeast, consoleApi, needs, qk, useConsoleScope } from "../../lib/console";
import { ago, when } from "../../lib/format";
import { navigate } from "../../lib/router";
import { setSession } from "../../lib/session";

function AuditLog({ org }: { org: string }) {
  const members = useQuery({ queryKey: qk.members(org), queryFn: () => consoleApi.members(org) });
  const who = (id: string) => members.data?.members.find((m) => m.user_id === id)?.name || id;
  const q = useInfiniteQuery({
    queryKey: qk.audit(org),
    queryFn: ({ pageParam }) => consoleApi.audit(org, pageParam || undefined),
    initialPageParam: 0,
    getNextPageParam: (last) => last.next ?? undefined,
  });
  const entries = q.data?.pages.flatMap((p) => p.entries) ?? [];
  if (q.error) return <ErrorState error={q.error} retry={() => void q.refetch()} />;
  if (q.isLoading) return <Skeleton className="h-32" />;
  if (!entries.length) return <p className="text-[13px] text-ink-3">Nothing recorded yet.</p>;
  return (
    <>
      <div className="overflow-x-auto overscroll-x-contain">
        <table className="stack-table w-full border-collapse text-[13px]">
          <thead>
            <tr className="border-b border-hairline">
              <th className={th}>When</th>
              <th className={th}>Action</th>
              <th className={th}>Actor</th>
              <th className={th}>Target</th>
              <th className={th}>Detail</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((e) => (
              <tr key={e.seq} className="border-b border-hairline last:border-0">
                <td className={cx(td, "whitespace-nowrap text-ink-2")} data-label="When" title={when(e.ts)}>
                  {ago(e.ts)}
                </td>
                <td className={td} data-primary>
                  <span className="font-mono text-[12.5px]">{e.action}</span>
                </td>
                <td className={cx(td, "text-ink-2")} data-label="Actor">
                  <span className="block max-w-[160px] truncate" title={e.actor}>
                    {who(e.actor)}
                  </span>
                </td>
                <td className={cx(td, "text-ink-2")} data-label="Target">
                  <span className="block max-w-[160px] truncate font-mono text-[12px]">{e.target ?? "–"}</span>
                </td>
                <td className={cx(td, "max-w-[320px] text-ink-3")} data-label="Detail">
                  <span className="block truncate font-mono text-[12px]" title={JSON.stringify(e.detail)}>
                    {Object.keys(e.detail ?? {}).length ? JSON.stringify(e.detail) : ""}
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {q.hasNextPage ? (
        <button className="mt-3 text-[13px] text-accent hover:underline" onClick={() => void q.fetchNextPage()} disabled={q.isFetchingNextPage}>
          {q.isFetchingNextPage ? "Loading…" : "Older entries"}
        </button>
      ) : null}
    </>
  );
}

export default function SettingsPage() {
  const qc = useQueryClient();
  const { project, org, role, orgRole } = useConsoleScope();
  const [name, setName] = useState("");
  const [workspace, setWorkspace] = useState("");
  const [retention, setRetention] = useState("");
  const [capture, setCapture] = useState(true);
  const [orgName, setOrgName] = useState("");
  const [deleting, setDeleting] = useState(false);
  useEffect(() => {
    if (!project) return;
    setName(project.name);
    setWorkspace(typeof project.settings.default_workspace === "string" ? project.settings.default_workspace : "");
    setRetention(project.settings.trace_retention_days ? String(project.settings.trace_retention_days) : "");
    setCapture(project.settings.capture_content !== false);
  }, [project]);
  useEffect(() => setOrgName(org?.name ?? ""), [org]);

  const save = useMutation({
    mutationFn: () => {
      const days = retention.trim() ? Number(retention) : null;
      if (days !== null && !(Number.isInteger(days) && days >= 1 && days <= 400)) throw new Error("Trace retention is a whole number of days, 1 to 400");
      return consoleApi.updateProject(project!.ref, {
        name: name.trim(),
        settings: { default_workspace: workspace.trim() || null, trace_retention_days: days, capture_content: capture },
      });
    },
    onSuccess: async (p) => {
      await qc.invalidateQueries({ queryKey: ["console", "me"] });
      selectProject(p);
    },
  });
  const rename = useMutation({ mutationFn: () => consoleApi.renameOrg(org!.id, orgName.trim()), onSuccess: () => qc.invalidateQueries({ queryKey: ["console", "me"] }) });
  const remove = useMutation({
    mutationFn: () => consoleApi.deleteProject(project!.ref, project!.name),
    onSuccess: async () => {
      setDeleting(false);
      setSession({ tenant: undefined, workspace: undefined });
      await qc.invalidateQueries({ queryKey: ["console"] });
      navigate(["projects"]);
    },
  });
  if (!project) return null;
  const admin = atLeast(role, "admin");

  return (
    <>
      <PageHeader title="Settings" context={`${project.name} · ${project.ref}`} />
      <Page>
        <div className="mx-auto flex max-w-3xl flex-col gap-3">
          <form
            onSubmit={(e) => {
              e.preventDefault();
              if (admin) save.mutate();
            }}
          >
            <Card title="Project" aside={!admin ? <span className="text-xs text-ink-3">{needs("admin")} to change</span> : null}>
              <fieldset disabled={!admin} className="grid gap-4 sm:grid-cols-2">
                <Field label="Name">{(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} maxLength={100} required />}</Field>
                <Field label="Default workspace" hint="The console opens the ontology pages in it; empty uses the server's">
                  {(id) => <Input id={id} value={workspace} onChange={(e) => setWorkspace(e.target.value)} placeholder="main" />}
                </Field>
                <Field label="Trace retention (days)" hint="1 to 400; empty uses the server's [traces] retention_days">
                  {(id) => <Input id={id} inputMode="numeric" value={retention} onChange={(e) => setRetention(e.target.value)} placeholder="Server default" />}
                </Field>
                <div className="flex flex-col justify-center gap-1">
                  <Checkbox label="Capture content" checked={capture} onChange={setCapture} disabled={!admin} />
                  <p className="pl-6 text-xs text-ink-3">Store questions, prompts and answers in traces (clipped). Off: metadata and token counts only.</p>
                </div>
              </fieldset>
              <div className="mt-4 flex items-center justify-end gap-3">
                {save.error ? (
                  <p className="mr-auto text-[13px] text-danger">{errorText(save.error)}</p>
                ) : save.isSuccess ? (
                  <p className="mr-auto text-[13px] text-ink-2">Saved.</p>
                ) : null}
                <RoleButton type="submit" variant="primary" size="sm" role={role} min="admin" busy={save.isPending}>
                  Save
                </RoleButton>
              </div>
            </Card>
          </form>
          {org ? (
            <form onSubmit={(e) => (e.preventDefault(), orgRole === "owner" && orgName.trim() && rename.mutate())}>
              <Card title="Organization" aside={orgRole !== "owner" ? <span className="text-xs text-ink-3">{needs("owner")} to rename</span> : null}>
                <div className="flex flex-wrap items-end gap-3">
                  <div className="min-w-0 flex-1 basis-56">
                    <Field label="Name" hint={`Slug ${org.slug}`}>
                      {(id) => <Input id={id} value={orgName} onChange={(e) => setOrgName(e.target.value)} disabled={orgRole !== "owner"} maxLength={100} />}
                    </Field>
                  </div>
                  <RoleButton type="submit" size="sm" role={orgRole} min="owner" busy={rename.isPending} disabled={!orgName.trim() || orgName.trim() === org.name}>
                    Rename
                  </RoleButton>
                </div>
                {rename.error ? <p className="mt-2 text-[13px] text-danger">{errorText(rename.error)}</p> : null}
              </Card>
            </form>
          ) : null}
          {org && atLeast(orgRole, "admin") ? (
            <Card title="Audit log" aside={<span className="text-xs text-ink-3">Every control-plane change in {org.name}</span>}>
              <AuditLog org={org.id} />
            </Card>
          ) : null}
          <Card title="Delete project" className="shadow-[0_0_0_1px_var(--danger-tint)]">
            <div className="flex flex-wrap items-center gap-3">
              <p className="min-w-0 flex-1 basis-64 text-[13px] text-ink-2">
                Deletes the ontology, its traces, keys and LLM settings. Every API key stops working. This cannot be undone.
              </p>
              <RoleButton variant="danger" size="sm" role={role} min="owner" onClick={() => setDeleting(true)}>
                Delete project
              </RoleButton>
            </div>
          </Card>
        </div>
      </Page>
      <ConfirmDialog
        open={deleting}
        onClose={() => setDeleting(false)}
        title={`Delete ${project.name}`}
        confirm="Delete project"
        type={project.name}
        busy={remove.isPending}
        error={remove.error ? errorText(remove.error) : undefined}
        onConfirm={() => remove.mutate()}
      >
        <p>
          The PostgreSQL schema of <span className="font-mono text-ink">{project.ref}</span> is dropped with everything in it.
        </p>
      </ConfirmDialog>
    </>
  );
}
