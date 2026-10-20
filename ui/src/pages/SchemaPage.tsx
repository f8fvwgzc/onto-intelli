import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { History, Plus, Undo2 } from "lucide-react";

import { PlanView } from "../components/PlanView";
import { ActionTypeEditor, InterfaceEditor, LinkTypeEditor, ObjectTypeEditor, RuleEditor } from "../components/SchemaEditors";
import { PageHeader } from "../components/Shell";
import { Badge, Button, Dialog, Dot, EmptyState, ErrorState, Field, Input, Segmented, Spinner, Textarea, cx } from "../components/ui";
import { call, errorText } from "../lib/api";
import { ago, kindColor, plural } from "../lib/format";
import { useSchema, useScope, useWhoami } from "../lib/queries";
import { href, navigate, useRoute } from "../lib/router";
import type { Json, SchemaChange, SchemaPlan, SchemaVersionInfo } from "../lib/types";

type Section = "object_types" | "link_types" | "action_types" | "rules" | "interfaces";

const SECTIONS: { value: Section; label: string; put: string; del: string }[] = [
  { value: "object_types", label: "Types", put: "put_object_type", del: "delete_object_type" },
  { value: "link_types", label: "Links", put: "put_link_type", del: "delete_link_type" },
  { value: "action_types", label: "Actions", put: "put_action_type", del: "delete_action_type" },
  { value: "rules", label: "Rules", put: "put_rule", del: "delete_rule" },
  { value: "interfaces", label: "Interfaces", put: "put_interface", del: "delete_interface" },
];

interface Pending {
  section: Section;
  name: string;
  change: SchemaChange;
}

export function SchemaPage() {
  const route = useRoute();
  const schemaQ = useSchema();
  const schema = schemaQ.data;
  const me = useWhoami().data;
  const qc = useQueryClient();
  const scope = useScope();
  const showHistory = route.path[1] === "history";
  const section = (!showHistory && (route.path[1] as Section)) || "object_types";
  const item = showHistory ? undefined : route.path[2];
  const [filter, setFilter] = useState("");
  const [pending, setPending] = useState<Pending[]>([]);
  const [preview, setPreview] = useState(false);
  const canEdit = !!(me?.capabilities.apply_schema || me?.capabilities.propose);

  const items = useMemo(() => {
    if (!schema) return [];
    const names = section === "rules" ? schema.rules.map((r) => r.name) : Object.keys(schema[section] ?? {});
    const added = pending.filter((p) => p.section === section && !names.includes(p.name) && !p.change.op.startsWith("delete")).map((p) => p.name);
    return [...names, ...added].filter((n) => !filter || n.includes(filter.toLowerCase())).sort();
  }, [schema, section, filter, pending]);

  const stage = (name: string, change: SchemaChange) => {
    setPending((list) => [...list.filter((p) => !(p.section === section && p.name === name)), { section, name, change }]);
    navigate(["schema", section, name]);
  };
  const meta = SECTIONS.find((s) => s.value === section)!;
  const onStage = (name: string, def: unknown) =>
    stage(name, section === "rules" ? { op: meta.put, def: def as Json } : { op: meta.put, name, def: def as Json });
  const onDelete = (name: string) => stage(name, { op: meta.del, name });

  // The draft shown for an item: its pending version if any, else the schema's.
  const initialOf = (name: string | undefined): unknown => {
    if (!name || !schema) return undefined;
    const p = pending.find((x) => x.section === section && x.name === name && !x.change.op.startsWith("delete"));
    if (p) return p.change.def;
    if (section === "rules") return schema.rules.find((r) => r.name === name);
    return (schema[section] as Record<string, unknown>)?.[name];
  };
  const initial = useMemo(() => initialOf(item === "new" ? undefined : item), [item, schema, section, pending]);

  if (schemaQ.isLoading) return <Centered><Spinner label="Loading the schema" /></Centered>;
  if (schemaQ.error || !schema) return <div className="p-5"><ErrorState error={schemaQ.error} retry={() => schemaQ.refetch()} /></div>;

  const editorProps = {
    schema,
    canEdit,
    onStage,
    onDelete,
    name: item === "new" ? undefined : item,
  };
  const pendingFor = (name: string) => pending.find((p) => p.section === section && p.name === name);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Schema"
        context={`${schema.name} · version ${schema.schema_version ?? "?"} · ${plural(Object.keys(schema.object_types).length, "type")}, ${plural(Object.keys(schema.link_types).length, "link type")}`}
      >
        <a href={href(["schema", "history"])} className="inline-flex h-8 items-center gap-1.5 rounded-input px-2.5 text-[13px] font-medium text-ink-2 hover:bg-sunken hover:text-ink">
          <History className="size-4" aria-hidden /> Versions
        </a>
      </PageHeader>
      <div className="flex min-h-0 flex-1 flex-col md:flex-row">
        <aside className={cx("flex min-h-0 flex-col border-hairline md:w-[280px] md:border-r", (item || showHistory) && "hidden md:flex")}>
          <div className="flex flex-col gap-2 p-3">
            <div className="overflow-x-auto">
              <Segmented<Section> label="Schema section" value={showHistory ? ("" as Section) : section} onChange={(s) => navigate(["schema", s])} options={SECTIONS.map(({ value, label }) => ({ value, label }))} />
            </div>
            <div className="flex gap-2">
              <Input aria-label="Filter" placeholder="Filter" value={filter} onChange={(e) => setFilter(e.target.value)} className="h-8" />
              <Button size="sm" icon={<Plus className="size-4" />} disabled={!canEdit} onClick={() => navigate(["schema", section, "new"])}>
                New
              </Button>
            </div>
          </div>
          <ul className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
            {items.map((name) => {
              const p = pendingFor(name);
              return (
                <li key={name}>
                  <a
                    href={href(["schema", section, name])}
                    aria-current={item === name ? "page" : undefined}
                    className={cx("flex h-8 items-center gap-2 rounded-input px-2 text-[13px]", item === name ? "bg-sunken text-ink" : "text-ink-2 hover:bg-sunken hover:text-ink")}
                  >
                    {section === "object_types" ? <Dot color={kindColor(name, schema)} /> : null}
                    <span className="min-w-0 flex-1 truncate font-mono">{name}</span>
                    {p ? <Badge tone={p.change.op.startsWith("delete") ? "danger" : "accent"}>{p.change.op.startsWith("delete") ? "delete" : "edited"}</Badge> : null}
                  </a>
                </li>
              );
            })}
            {items.length === 0 ? <li className="px-2 py-4 text-[13px] text-ink-3">Nothing here yet.</li> : null}
          </ul>
        </aside>
        <div className={cx("min-h-0 flex-1 overflow-y-auto", pending.length > 0 && "pb-20", !item && !showHistory && "hidden md:block")}>
          {item ? (
            <>
              <div className="px-4 pt-3 md:hidden">
                <a href={href(["schema", section])} className="text-[13px] text-ink-2">
                  ← {meta.label}
                </a>
              </div>
              {section === "object_types" ? <ObjectTypeEditor {...editorProps} initial={initial as never} /> : null}
              {section === "link_types" ? <LinkTypeEditor {...editorProps} initial={initial as never} /> : null}
              {section === "action_types" ? <ActionTypeEditor {...editorProps} initial={initial as never} /> : null}
              {section === "rules" ? <RuleEditor {...editorProps} initial={initial as never} /> : null}
              {section === "interfaces" ? <InterfaceEditor {...editorProps} initial={initial as never} /> : null}
            </>
          ) : showHistory ? (
            <VersionHistory canApply={!!me?.capabilities.apply_schema} />
          ) : (
            <EmptyState title="Pick something to edit">
              Changes collect in a change set. Preview it to see what it does to the data, then apply it or file it as a proposal.
            </EmptyState>
          )}
        </div>
      </div>

      {pending.length ? (
        <div className="fixed inset-x-0 bottom-14 z-10 border-t border-hairline bg-raised px-4 py-2.5 shadow-overlay md:static md:bottom-0">
          <div className="flex flex-wrap items-center gap-2">
            <p className="min-w-0 flex-1 text-[13px]">
              <span className="font-medium">{plural(pending.length, "change")} staged</span>
              <span className="text-ink-3"> · {pending.map((p) => `${p.change.op.replace(/_/g, " ")} ${p.name}`).join(", ")}</span>
            </p>
            <Button size="sm" variant="ghost" onClick={() => setPending([])}>
              Discard
            </Button>
            <Button size="sm" variant="primary" onClick={() => setPreview(true)}>
              Preview impact
            </Button>
          </div>
        </div>
      ) : null}

      {preview ? (
        <PreviewDialog
          changes={pending.map((p) => p.change)}
          baseVersion={schema.schema_version}
          canApply={!!me?.capabilities.apply_schema}
          canPropose={!!me?.capabilities.propose}
          onClose={() => setPreview(false)}
          onDone={(what) => {
            setPreview(false);
            setPending([]);
            void qc.invalidateQueries({ queryKey: ["schema", ...scope] });
            void qc.invalidateQueries({ queryKey: ["proposals"] });
            void qc.invalidateQueries({ queryKey: ["overview"] });
            if (what.proposal) navigate(["proposals", what.proposal]);
          }}
        />
      ) : null}
    </div>
  );
}

function Centered({ children }: { children: React.ReactNode }) {
  return <div className="grid min-h-0 flex-1 place-items-center">{children}</div>;
}

function PreviewDialog({ changes, baseVersion, canApply, canPropose, onClose, onDone }: { changes: SchemaChange[]; baseVersion?: number; canApply: boolean; canPropose: boolean; onClose: () => void; onDone: (r: { proposal?: string }) => void }) {
  const [migrations, setMigrations] = useState("[]");
  const [title, setTitle] = useState("");
  const [rationale, setRationale] = useState("");
  const [mode, setMode] = useState<"review" | "propose">("review");
  let parsed: Json[] = [];
  let migErr: string | undefined;
  try {
    const v = JSON.parse(migrations || "[]");
    if (!Array.isArray(v)) throw new Error("must be a list");
    parsed = v;
  } catch (e) {
    migErr = (e as Error).message;
  }
  const set = { changes, migrations: parsed, expected_version: baseVersion };
  const plan = useQuery<SchemaPlan>({
    queryKey: ["plan", JSON.stringify(set)],
    queryFn: () => call<SchemaPlan>("schema.plan", set),
    enabled: !migErr,
  });
  const apply = useMutation({ mutationFn: () => call("schema.apply", { ...set, message: title || undefined }), onSuccess: () => onDone({}) });
  const propose = useMutation({
    mutationFn: () => call<{ id: string }>("schema.propose", { ...set, title, rationale, source: "ui" }),
    onSuccess: (r) => onDone({ proposal: r.id }),
  });
  const p = plan.data;
  const applicable = !!p && p.valid && !(p.needs_migration ?? []).length;
  const err = apply.error ?? propose.error;

  return (
    <Dialog
      open
      wide
      onClose={onClose}
      title={mode === "review" ? "Preview the change set" : "File a proposal"}
      footer={
        mode === "review" ? (
          <>
            {err ? <p className="mr-auto self-center text-[13px] text-danger">{errorText(err)}</p> : null}
            <Button variant="ghost" onClick={onClose}>
              Keep editing
            </Button>
            {canPropose ? (
              <Button disabled={!p?.valid} onClick={() => setMode("propose")}>
                Propose for review
              </Button>
            ) : null}
            {canApply ? (
              <Button variant="primary" busy={apply.isPending} disabled={!applicable} onClick={() => apply.mutate()}>
                Apply as version {(baseVersion ?? 0) + 1}
              </Button>
            ) : null}
          </>
        ) : (
          <>
            {err ? <p className="mr-auto self-center text-[13px] text-danger">{errorText(err)}</p> : null}
            <Button variant="ghost" onClick={() => setMode("review")}>
              Back
            </Button>
            <Button variant="primary" busy={propose.isPending} disabled={!title.trim()} onClick={() => propose.mutate()}>
              File proposal
            </Button>
          </>
        )
      }
    >
      {mode === "review" ? (
        <div className="flex flex-col gap-5">
          {plan.isLoading ? <Spinner label="Working out the impact" /> : plan.error ? <ErrorState error={plan.error} /> : p ? <PlanView plan={p} /> : null}
          <Field label="Migrations" hint='For breaking changes, e.g. [{"action": "map_states", "kind": "issue", "map": {"triaged": "open"}}]. See docs/SCHEMA.md.' error={migErr}>
            {(id) => <Textarea id={id} spellCheck={false} className="min-h-20 font-mono text-[12.5px]" value={migrations} onChange={(e) => setMigrations(e.target.value)} />}
          </Field>
          {!canApply && canPropose ? <p className="text-[13px] text-ink-2">You can propose this change; an approver applies it.</p> : null}
        </div>
      ) : (
        <div className="flex flex-col gap-4">
          <Field label="Title">{(id) => <Input id={id} value={title} onChange={(e) => setTitle(e.target.value)} autoFocus />}</Field>
          <Field label="Rationale" hint="Why it is needed, and the evidence for it">
            {(id) => <Textarea id={id} value={rationale} onChange={(e) => setRationale(e.target.value)} />}
          </Field>
          {p ? <PlanView plan={p} /> : null}
        </div>
      )}
    </Dialog>
  );
}

function VersionHistory({ canApply }: { canApply: boolean }) {
  const scope = useScope();
  const qc = useQueryClient();
  const history = useQuery<{ active: number; versions: SchemaVersionInfo[] }>({
    queryKey: ["schema-history", ...scope],
    queryFn: () => call("schema.history", { limit: 100 }),
  });
  const [confirm, setConfirm] = useState<{ action: "schema.undo" | "schema.revert"; version: number } | null>(null);
  return (
    <div className="mx-auto w-full max-w-[760px] px-4 py-5 md:px-6">
      <h2 className="text-lg font-semibold tracking-tight">Versions</h2>
      <p className="mt-0.5 text-[13px] text-ink-2">Every change is a version with its document and diff. Undo one, or make an earlier one active again.</p>
      {history.isLoading ? (
        <div className="mt-6"><Spinner /></div>
      ) : history.error ? (
        <div className="mt-6"><ErrorState error={history.error} /></div>
      ) : (
        <ol className="mt-5 flex flex-col">
          {history.data!.versions.map((v) => (
            <li key={v.version} className="flex gap-3 border-b border-hairline py-3 last:border-0">
              <span className={cx("tnum mt-0.5 w-10 shrink-0 font-mono text-[13px]", v.version === history.data!.active ? "font-semibold text-accent" : "text-ink-3")}>v{v.version}</span>
              <div className="min-w-0 flex-1">
                <p className="text-[13px] text-ink">
                  {v.message || <span className="text-ink-3">No message</span>}
                  {v.breaking ? <Badge tone="warning" className="ml-2">breaking</Badge> : null}
                </p>
                <p className="text-xs text-ink-3">
                  {v.source} · {v.actor} · {ago(v.ts)}
                  {v.proposal ? (
                    <>
                      {" · "}
                      <a className="underline-offset-2 hover:underline" href={href(["proposals", v.proposal])}>
                        proposal
                      </a>
                    </>
                  ) : null}
                </p>
                {v.diff.length ? <p className="mt-1 truncate font-mono text-xs text-ink-2">{v.diff.join("  ")}</p> : null}
              </div>
              {canApply && v.parent ? (
                <div className="flex shrink-0 gap-1">
                  <Button size="sm" variant="ghost" icon={<Undo2 className="size-3.5" />} onClick={() => setConfirm({ action: "schema.undo", version: v.version })}>
                    Undo
                  </Button>
                  {v.version !== history.data!.active ? (
                    <Button size="sm" variant="ghost" onClick={() => setConfirm({ action: "schema.revert", version: v.version })}>
                      Restore
                    </Button>
                  ) : null}
                </div>
              ) : null}
            </li>
          ))}
        </ol>
      )}
      {confirm ? (
        <RevertDialog
          {...confirm}
          onClose={() => setConfirm(null)}
          onDone={() => {
            setConfirm(null);
            void qc.invalidateQueries();
          }}
        />
      ) : null}
    </div>
  );
}

function RevertDialog({ action, version, onClose, onDone }: { action: "schema.undo" | "schema.revert"; version: number; onClose: () => void; onDone: () => void }) {
  const dry = useQuery<{ plan?: SchemaPlan }>({ queryKey: ["revert", action, version], queryFn: () => call(action, { version, dry_run: true }), retry: false });
  const run = useMutation({ mutationFn: () => call(action, { version }), onSuccess: onDone });
  const p = dry.data?.plan;
  return (
    <Dialog
      open
      wide
      onClose={onClose}
      title={action === "schema.undo" ? `Undo version ${version}` : `Restore version ${version}`}
      footer={
        <>
          {run.error ? <p className="mr-auto self-center text-[13px] text-danger">{errorText(run.error)}</p> : null}
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" busy={run.isPending} disabled={!p || !p.valid || !!(p.needs_migration ?? []).length} onClick={() => run.mutate()}>
            {action === "schema.undo" ? "Undo it" : "Restore it"}
          </Button>
        </>
      }
    >
      {dry.isLoading ? <Spinner label="Working out the impact" /> : dry.error ? <ErrorState error={dry.error} /> : p ? <PlanView plan={p} /> : null}
      <p className="mt-4 text-[13px] text-ink-2">This restores the schema only: entities removed by an earlier migration stay removed, their history remains.</p>
    </Dialog>
  );
}

