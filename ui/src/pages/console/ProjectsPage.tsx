import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus } from "lucide-react";

import { Sparkline } from "../../components/charts";
import { Page, RoleButton } from "../../components/consoleUi";
import { selectProject } from "../../components/ProjectSwitcher";
import { PageHeader } from "../../components/Shell";
import { Button, Checkbox, Dialog, EmptyState, ErrorState, Field, Input, Select, Skeleton } from "../../components/ui";
import { errorText } from "../../lib/api";
import { atLeast, consoleApi, qk, useConsoleScope, type Org, type Project } from "../../lib/console";
import { ago, num } from "../../lib/format";
import { DAY, observe } from "../../lib/observe";
import { navigate, setQuery, useRoute } from "../../lib/router";
import { useSession } from "../../lib/session";

/** Now, to the minute: query keys stay stable between renders. */
export function minuteNow(): number {
  return Math.floor(Date.now() / 60_000) * 60_000;
}

function ProjectCard({ p }: { p: Project }) {
  const session = useSession();
  const until = minuteNow() + 60_000;
  const m = useQuery({
    queryKey: ["metrics", p.ref, "7d", until],
    queryFn: () => observe.metrics({ since: until - 7 * DAY, until, bucket: "day" }, p.ref),
    staleTime: 60_000,
  });
  const current = p.ref === session.tenant;
  return (
    <li>
      <button
        onClick={() => {
          selectProject(p);
          navigate(["overview"]);
        }}
        className="flex h-full w-full flex-col rounded-card bg-raised p-4 text-left shadow-card transition-shadow duration-150 hover:shadow-overlay"
      >
        <span className="flex w-full items-center gap-2">
          <span className="min-w-0 flex-1 truncate font-medium text-ink">{p.name}</span>
          {current ? <span className="text-xs text-accent">Current</span> : null}
        </span>
        <span className="mt-0.5 font-mono text-xs text-ink-3">{p.ref}</span>
        <span className="mt-4 flex w-full items-end gap-3">
          <span className="min-w-0 flex-1">
            <span className="tnum block text-lg font-semibold tracking-tight text-ink">{m.data ? num(m.data.requests) : m.isLoading ? "…" : "–"}</span>
            <span className="block text-xs text-ink-3">requests, last 7 days</span>
          </span>
          <span className="w-24 shrink-0">{m.data ? <Sparkline values={m.data.series.map((s) => s.requests)} /> : null}</span>
        </span>
        <span className="mt-3 text-xs text-ink-3">Created {ago(p.created_at)}</span>
      </button>
    </li>
  );
}

function NewProjectDialog({ open, onClose, orgs, defaultOrg }: { open: boolean; onClose: () => void; orgs: Org[]; defaultOrg?: string }) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [seed, setSeed] = useState(true);
  const [org, setOrg] = useState(defaultOrg ?? orgs[0]?.id ?? "");
  useEffect(() => {
    if (open) {
      setName("");
      setOrg(defaultOrg ?? orgs[0]?.id ?? "");
    }
  }, [open, defaultOrg, orgs]);
  const create = useMutation({
    mutationFn: () => consoleApi.createProject(org, name.trim(), seed ? "demo" : undefined),
    onSuccess: async (p) => {
      await qc.invalidateQueries({ queryKey: ["console"] });
      selectProject(p);
      onClose();
      navigate(["overview"]);
    },
  });
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="New project"
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" type="submit" form="new-project" busy={create.isPending} disabled={!name.trim() || !org}>
            Create project
          </Button>
        </>
      }
    >
      <form
        id="new-project"
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (name.trim() && org) create.mutate();
        }}
      >
        <Field label="Name" error={create.error ? errorText(create.error) : undefined}>
          {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} maxLength={100} required autoFocus placeholder="Payments platform" />}
        </Field>
        {orgs.length > 1 ? (
          <Field label="Organization">
            {(id) => (
              <Select id={id} value={org} onChange={(e) => setOrg(e.target.value)}>
                {orgs.map((o) => (
                  <option key={o.id} value={o.id}>
                    {o.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        ) : null}
        <div>
          <Checkbox label="Load the demo data" checked={seed} onChange={setSeed} />
          <p className="mt-1 pl-6 text-xs text-ink-3">Teams, people, projects and services to try questions on. Leave it off for an empty ontology.</p>
        </div>
      </form>
    </Dialog>
  );
}

function NewOrgDialog({ open, onClose, onCreated }: { open: boolean; onClose: () => void; onCreated: (org: Org) => void }) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const create = useMutation({
    mutationFn: () => consoleApi.createOrg(name.trim()),
    onSuccess: async (o) => {
      await qc.invalidateQueries({ queryKey: ["console"] });
      onCreated(o);
      setName("");
      onClose();
    },
  });
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="New organization"
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" type="submit" form="new-org" busy={create.isPending} disabled={!name.trim()}>
            Create organization
          </Button>
        </>
      }
    >
      <form id="new-org" onSubmit={(e) => (e.preventDefault(), name.trim() && create.mutate())}>
        <Field label="Name" hint="You become its owner." error={create.error ? errorText(create.error) : undefined}>
          {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} maxLength={100} required autoFocus />}
        </Field>
      </form>
    </Dialog>
  );
}

export default function ProjectsPage() {
  const route = useRoute();
  const { me, org: current } = useConsoleScope();
  // The organization on view: ?org= (the select), else the current project's.
  const org = me?.orgs.find((o) => o.id === route.query.get("org")) ?? current;
  const [orgDialog, setOrgDialog] = useState(false);
  const projects = useQuery({ queryKey: qk.projects(org?.id ?? ""), queryFn: () => consoleApi.projects(org!.id), enabled: Boolean(org) });
  const adminOrgs = (me?.orgs ?? []).filter((o) => atLeast(o.role, "admin"));
  const dialog = route.query.get("new") === "1";

  return (
    <>
      <PageHeader title="Projects" context={org ? `${org.name} · you are ${org.role}` : "No organization"}>
        {me && me.orgs.length > 1 ? (
          <Select aria-label="Organization" value={org?.id} onChange={(e) => setQuery({ org: e.target.value })} className="h-8 max-w-44 text-[13px]">
            {me.orgs.map((o) => (
              <option key={o.id} value={o.id}>
                {o.name}
              </option>
            ))}
          </Select>
        ) : null}
        <Button size="sm" variant="ghost" onClick={() => setOrgDialog(true)}>
          New organization
        </Button>
        <RoleButton size="sm" variant="primary" role={org?.role} min="admin" icon={<Plus className="size-4" aria-hidden />} onClick={() => setQuery({ new: "1" })}>
          New project
        </RoleButton>
      </PageHeader>
      <Page>
        {projects.error ? (
          <ErrorState error={projects.error} retry={() => void projects.refetch()} />
        ) : projects.isLoading || !org ? (
          org ? (
            <ul className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
              {[0, 1, 2].map((i) => (
                <Skeleton key={i} className="h-36 rounded-card" />
              ))}
            </ul>
          ) : (
            <EmptyState title="No organization">Create an organization to hold your projects, or ask for an invitation to one.</EmptyState>
          )
        ) : projects.data?.projects.length ? (
          <ul className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
            {projects.data.projects.map((p) => (
              <ProjectCard key={p.id} p={p} />
            ))}
          </ul>
        ) : (
          <EmptyState
            title="No projects in this organization"
            action={
              <RoleButton variant="primary" role={org.role} min="admin" onClick={() => setQuery({ new: "1" })}>
                New project
              </RoleButton>
            }
          >
            A project is one ontology with its own API keys, traces and LLM settings.
          </EmptyState>
        )}
      </Page>
      <NewProjectDialog
        open={dialog && adminOrgs.length > 0}
        onClose={() => setQuery({ new: undefined })}
        orgs={adminOrgs}
        defaultOrg={adminOrgs.some((o) => o.id === org?.id) ? org?.id : undefined}
      />
      <NewOrgDialog open={orgDialog} onClose={() => setOrgDialog(false)} onCreated={(o) => setQuery({ org: o.id })} />
    </>
  );
}
