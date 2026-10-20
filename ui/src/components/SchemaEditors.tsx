import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Plus, Trash2 } from "lucide-react";

import type { ActionType, InterfaceType, Json, LinkType, ObjectType, ParamDef, PropertyDef, Rule, SchemaDoc } from "../lib/types";
import { Button, Checkbox, Field, IconButton, Input, Select, TagInput, Textarea, cx } from "./ui";

export const NAME_RE = /^[a-z][a-z0-9_]*$/;
const PROP_TYPES = ["string", "text", "int", "float", "bool", "timestamp", "enum", "list", "json"];
const PARAM_TYPES = ["entity", "string", "text", "int", "float", "bool", "enum"];
const CATEGORIES = ["thing", "actor", "container", "artifact", "knowledge"];

export function nameError(name: string, taken: string[], isNew: boolean): string | undefined {
  if (!isNew) return undefined;
  if (!name) return "Required";
  if (!NAME_RE.test(name)) return "Lowercase letters, digits and _, starting with a letter";
  if (taken.includes(name)) return "Already exists";
  return undefined;
}

/** Strip empty optional fields so diffs stay meaningful. */
function clean<T extends object>(o: T): T {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(o)) {
    if (v === undefined || v === "" || (Array.isArray(v) && v.length === 0)) continue;
    if (v && typeof v === "object" && !Array.isArray(v) && Object.keys(v).length === 0) continue;
    out[k] = v;
  }
  return out as T;
}

export interface EditorProps<T> {
  schema: SchemaDoc;
  name?: string;
  initial?: T;
  onStage: (name: string, def: T) => void;
  onDelete?: (name: string) => void;
  canEdit: boolean;
}

function EditorFrame({ title, subtitle, children, actions }: { title: string; subtitle?: string; children: ReactNode; actions: ReactNode }) {
  return (
    <div className="mx-auto w-full max-w-[760px] px-4 py-5 md:px-6">
      <div className="mb-5">
        <h2 className="text-lg font-semibold tracking-tight">{title}</h2>
        {subtitle ? <p className="mt-0.5 text-[13px] text-ink-2">{subtitle}</p> : null}
      </div>
      <div className="flex flex-col gap-5">{children}</div>
      <div className="mt-8 flex flex-wrap items-center gap-2 border-t border-hairline pt-4">{actions}</div>
    </div>
  );
}

function Grid({ children }: { children: ReactNode }) {
  return <div className="grid gap-4 sm:grid-cols-2">{children}</div>;
}

// ── properties table (object types and interfaces) ─────────────────────

function PropertyTable({ value, onChange, disabled }: { value: Record<string, PropertyDef>; onChange: (v: Record<string, PropertyDef>) => void; disabled: boolean }) {
  const [rows, setRows] = useState(() => Object.entries(value).map(([name, def]) => ({ name, def })));
  useEffect(() => setRows(Object.entries(value).map(([name, def]) => ({ name, def }))), [value]);
  const push = (next: { name: string; def: PropertyDef }[]) => {
    setRows(next);
    const out: Record<string, PropertyDef> = {};
    for (const r of next) if (r.name) out[r.name] = clean(r.def);
    onChange(out);
  };
  const update = (i: number, patch: Partial<{ name: string; def: PropertyDef }>) =>
    push(rows.map((r, j) => (j === i ? { name: patch.name ?? r.name, def: { ...r.def, ...(patch.def ?? {}) } } : r)));
  return (
    <div className="flex flex-col gap-2">
      {rows.length === 0 ? <p className="text-[13px] text-ink-3">No properties of its own.</p> : null}
      {rows.map((r, i) => (
        <div key={i} className="rounded-card bg-raised p-3 shadow-card">
          <div className="grid grid-cols-[1fr_auto] gap-2 sm:grid-cols-[minmax(0,1.2fr)_minmax(0,1fr)_auto]">
            <Input aria-label="Property name" placeholder="name" value={r.name} disabled={disabled} className={cx("font-mono", r.name && !NAME_RE.test(r.name) && "outline-2 outline-danger")} onChange={(e) => update(i, { name: e.target.value })} />
            <Select aria-label="Property type" value={r.def.type} disabled={disabled} className="order-3 col-span-2 sm:order-none sm:col-span-1" onChange={(e) => update(i, { def: { type: e.target.value } as PropertyDef })}>
              {PROP_TYPES.map((t) => (
                <option key={t}>{t}</option>
              ))}
            </Select>
            <IconButton label={`Remove ${r.name || "property"}`} disabled={disabled} onClick={() => push(rows.filter((_, j) => j !== i))}>
              <Trash2 className="size-4" />
            </IconButton>
          </div>
          {r.def.type === "enum" ? (
            <div className="mt-2">
              <TagInput value={r.def.values ?? []} onChange={(values) => update(i, { def: { values } as PropertyDef })} placeholder="Allowed values, Enter to add" />
            </div>
          ) : null}
          <div className="mt-2 grid gap-2 sm:grid-cols-[1fr_1fr_120px]">
            <Input aria-label="Description" placeholder="Description" value={r.def.description ?? ""} disabled={disabled} onChange={(e) => update(i, { def: { description: e.target.value } as PropertyDef })} />
            <Input aria-label="Pattern" placeholder="Pattern, e.g. ^[a-z]+$" className="font-mono" value={r.def.pattern ?? ""} disabled={disabled} onChange={(e) => update(i, { def: { pattern: e.target.value || undefined } as PropertyDef })} />
            <Input aria-label="Max length" placeholder="Max length" inputMode="numeric" value={r.def.max_length ?? ""} disabled={disabled} onChange={(e) => update(i, { def: { max_length: e.target.value ? Number(e.target.value) : undefined } as PropertyDef })} />
          </div>
          <div className="mt-2 flex flex-wrap gap-4">
            <Checkbox label="Required" checked={!!r.def.required} disabled={disabled} onChange={(v) => update(i, { def: { required: v || undefined } as PropertyDef })} />
            <Checkbox label="Searchable" checked={!!r.def.searchable} disabled={disabled} onChange={(v) => update(i, { def: { searchable: v || undefined } as PropertyDef })} />
          </div>
        </div>
      ))}
      <div>
        <Button size="sm" variant="ghost" icon={<Plus className="size-4" />} disabled={disabled} onClick={() => push([...rows, { name: "", def: { type: "string" } }])}>
          Add property
        </Button>
      </div>
    </div>
  );
}

// ── object types ───────────────────────────────────────────────────────

export function ObjectTypeEditor({ schema, name, initial, onStage, onDelete, canEdit }: EditorProps<ObjectType>) {
  const isNew = !name;
  const [n, setN] = useState(name ?? "");
  const [d, setD] = useState<ObjectType>(() => initial ?? { name: "", category: "thing" });
  useEffect(() => {
    setN(name ?? "");
    setD(initial ?? { name: "", category: "thing" });
  }, [name, initial]);
  const kinds = Object.keys(schema.object_types).filter((k) => k !== name);
  const err = nameError(n, Object.keys(schema.object_types), isNew);
  const set = (patch: Partial<ObjectType>) => setD((x) => ({ ...x, ...patch }));
  return (
    <EditorFrame
      title={isNew ? "New object type" : d.label || name!}
      subtitle={isNew ? "A kind of thing in the graph." : `object_types.${name}`}
      actions={
        <>
          <Button variant="primary" disabled={!canEdit || !!err} onClick={() => onStage(n, clean({ ...d, name: n }))}>
            {isNew ? "Add to change set" : "Stage changes"}
          </Button>
          {!isNew && onDelete ? (
            <Button variant="danger" icon={<Trash2 className="size-4" />} disabled={!canEdit} onClick={() => onDelete(name!)}>
              Delete type
            </Button>
          ) : null}
        </>
      }
    >
      <Grid>
        <Field label="Name" error={err} hint={isNew ? "Stored in ids: issue:421" : "Rename through a change set file (rename_object_type)."}>
          {(id) => <Input id={id} className="font-mono" value={n} disabled={!isNew || !canEdit} onChange={(e) => setN(e.target.value)} />}
        </Field>
        <Field label="Label">{(id) => <Input id={id} value={d.label ?? ""} disabled={!canEdit} onChange={(e) => set({ label: e.target.value })} />}</Field>
      </Grid>
      <Field label="Description">{(id) => <Textarea id={id} value={d.description ?? ""} disabled={!canEdit} onChange={(e) => set({ description: e.target.value })} />}</Field>
      <Grid>
        <Field label="Category">
          {(id) => (
            <Select id={id} value={d.category} disabled={!canEdit} onChange={(e) => set({ category: e.target.value })}>
              {CATEGORIES.map((c) => (
                <option key={c}>{c}</option>
              ))}
            </Select>
          )}
        </Field>
        <Field label="Extends" hint="Inherits properties and link rules">
          {(id) => (
            <Select id={id} value={d.extends ?? ""} disabled={!canEdit} onChange={(e) => set({ extends: e.target.value || undefined })}>
              <option value="">—</option>
              {kinds.map((k) => (
                <option key={k}>{k}</option>
              ))}
            </Select>
          )}
        </Field>
        <Field label="Colour">
          {(id) => (
            <div className="flex gap-2">
              <input aria-label="Pick colour" type="color" value={d.color && /^#[0-9a-f]{6}$/i.test(d.color) ? d.color : "#64748b"} disabled={!canEdit} onChange={(e) => set({ color: e.target.value })} className="h-9 w-11 shrink-0 cursor-pointer rounded-input bg-raised p-1 shadow-card" />
              <Input id={id} className="font-mono" placeholder="#rrggbb" value={d.color ?? ""} disabled={!canEdit} onChange={(e) => set({ color: e.target.value })} />
            </div>
          )}
        </Field>
        <Field label="Icon">{(id) => <Input id={id} placeholder="e.g. flag" value={d.icon ?? ""} disabled={!canEdit} onChange={(e) => set({ icon: e.target.value })} />}</Field>
      </Grid>
      <Field label="Implements" hint="Interfaces this type takes part in">
        {() => <TagInput value={d.implements ?? []} suggestions={Object.keys(schema.interfaces)} onChange={(implements_) => set({ implements: implements_ })} />}
      </Field>
      <Field label="States" hint="Empty: no state machine">
        {() => <TagInput value={d.states ?? []} onChange={(states) => set({ states })} placeholder="open, in_progress, done…" />}
      </Field>
      <Grid>
        <Field label="Terminal states" hint="Finished: not counted as open">
          {() => <TagInput value={d.terminal_states ?? []} suggestions={d.states} onChange={(terminal_states) => set({ terminal_states })} />}
        </Field>
        <Field label="Alert states" hint="Candidates in root-cause analysis">
          {() => <TagInput value={d.alert_states ?? []} suggestions={d.states} onChange={(alert_states) => set({ alert_states })} />}
        </Field>
      </Grid>
      <div>
        <p className="mb-2 text-[13px] font-medium">Properties</p>
        <PropertyTable value={d.properties ?? {}} disabled={!canEdit} onChange={(properties) => set({ properties })} />
      </div>
    </EditorFrame>
  );
}

// ── link types ─────────────────────────────────────────────────────────

export function LinkTypeEditor({ schema, name, initial, onStage, onDelete, canEdit }: EditorProps<LinkType>) {
  const isNew = !name;
  const blank: LinkType = { name: "", cardinality: "many", weight: 1 };
  const [n, setN] = useState(name ?? "");
  const [d, setD] = useState<LinkType>(() => initial ?? blank);
  useEffect(() => {
    setN(name ?? "");
    setD(initial ?? blank);
  }, [name, initial]);
  const kinds = useMemo(() => [...Object.keys(schema.object_types), ...Object.keys(schema.interfaces)], [schema]);
  const taken = [...Object.keys(schema.link_types), ...Object.values(schema.link_types).map((l) => l.inverse ?? "").filter(Boolean)];
  const err = nameError(n, taken, isNew);
  const set = (patch: Partial<LinkType>) => setD((x) => ({ ...x, ...patch }));
  const invErr = d.inverse && !NAME_RE.test(d.inverse) ? "Lowercase letters, digits and _" : undefined;
  return (
    <EditorFrame
      title={isNew ? "New link type" : d.label || name!}
      subtitle={isNew ? "A relation between two kinds, stored once and walked both ways." : `link_types.${name}`}
      actions={
        <>
          <Button variant="primary" disabled={!canEdit || !!err || !!invErr} onClick={() => onStage(n, clean({ ...d, name: n }))}>
            {isNew ? "Add to change set" : "Stage changes"}
          </Button>
          {!isNew && onDelete ? (
            <Button variant="danger" icon={<Trash2 className="size-4" />} disabled={!canEdit} onClick={() => onDelete(name!)}>
              Delete link type
            </Button>
          ) : null}
        </>
      }
    >
      <Grid>
        <Field label="Name" error={err}>
          {(id) => <Input id={id} className="font-mono" value={n} disabled={!isNew || !canEdit} onChange={(e) => setN(e.target.value)} />}
        </Field>
        <Field label="Inverse name" error={invErr} hint="The same relation walked backwards">
          {(id) => <Input id={id} className="font-mono" value={d.inverse ?? ""} disabled={!canEdit} onChange={(e) => set({ inverse: e.target.value || undefined })} />}
        </Field>
      </Grid>
      <Grid>
        <Field label="From" hint="Empty: any kind">
          {() => <TagInput value={d.from ?? []} suggestions={kinds} onChange={(from) => set({ from })} />}
        </Field>
        <Field label="To" hint="Empty: any kind">
          {() => <TagInput value={d.to ?? []} suggestions={kinds} onChange={(to) => set({ to })} />}
        </Field>
      </Grid>
      <Grid>
        <Field label="Cardinality" hint="One: linking again replaces the link">
          {(id) => (
            <Select id={id} value={d.cardinality} disabled={!canEdit} onChange={(e) => set({ cardinality: e.target.value as LinkType["cardinality"] })}>
              <option value="many">many</option>
              <option value="one">one</option>
            </Select>
          )}
        </Field>
        <Field label="Weight" hint="Relevance when building context, 0–1">
          {(id) => <Input id={id} type="number" min={0.05} max={1} step={0.05} value={d.weight} disabled={!canEdit} onChange={(e) => set({ weight: Number(e.target.value) })} />}
        </Field>
      </Grid>
      <Field label="Label">{(id) => <Input id={id} value={d.label ?? ""} disabled={!canEdit} onChange={(e) => set({ label: e.target.value })} />}</Field>
      <Field label="Description">{(id) => <Textarea id={id} value={d.description ?? ""} disabled={!canEdit} onChange={(e) => set({ description: e.target.value })} />}</Field>
      <div className="flex flex-wrap gap-5">
        <Checkbox label="Dependency (blocks, critical path)" checked={!!d.dependency} disabled={!canEdit} onChange={(v) => set({ dependency: v || undefined })} />
        <Checkbox label="Symmetric" checked={!!d.symmetric} disabled={!canEdit} onChange={(v) => set({ symmetric: v || undefined })} />
        <Checkbox label="Provenance" checked={!!d.provenance} disabled={!canEdit} onChange={(v) => set({ provenance: v || undefined })} />
      </div>
    </EditorFrame>
  );
}

// ── action types ───────────────────────────────────────────────────────

function JsonField({ label, hint, value, onChange, disabled }: { label: string; hint: string; value: Json[]; onChange: (v: Json[] | null) => void; disabled: boolean }) {
  const [text, setText] = useState(() => JSON.stringify(value, null, 2));
  const [err, setErr] = useState<string>();
  useEffect(() => setText(JSON.stringify(value, null, 2)), [value]);
  return (
    <Field label={label} hint={hint} error={err}>
      {(id) => (
        <Textarea
          id={id}
          spellCheck={false}
          disabled={disabled}
          className="min-h-36 font-mono text-[12.5px]"
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            try {
              const v = JSON.parse(e.target.value || "[]");
              if (!Array.isArray(v)) throw new Error("must be a JSON list");
              setErr(undefined);
              onChange(v);
            } catch (x) {
              setErr(`Not valid JSON yet: ${(x as Error).message}`);
              onChange(null);
            }
          }}
        />
      )}
    </Field>
  );
}

export function ActionTypeEditor({ schema, name, initial, onStage, onDelete, canEdit }: EditorProps<ActionType>) {
  const isNew = !name;
  const blank: ActionType = { name: "", risk: "low", parameters: {}, effects: [] };
  const [n, setN] = useState(name ?? "");
  const [d, setD] = useState<ActionType>(() => initial ?? blank);
  const [jsonOk, setJsonOk] = useState(true);
  useEffect(() => {
    setN(name ?? "");
    setD(initial ?? blank);
  }, [name, initial]);
  const err = nameError(n, Object.keys(schema.action_types), isNew);
  const set = (patch: Partial<ActionType>) => setD((x) => ({ ...x, ...patch }));
  const params = Object.entries(d.parameters ?? {});
  const kinds = [...Object.keys(schema.object_types), ...Object.keys(schema.interfaces)];
  const setParam = (old: string, next: string, def: ParamDef) => {
    const out: Record<string, ParamDef> = {};
    for (const [k, v] of params) {
      if (k === old) {
        if (next) out[next] = def;
      } else out[k] = v;
    }
    set({ parameters: out });
  };
  return (
    <EditorFrame
      title={isNew ? "New action type" : name!}
      subtitle="A governed edit: typed parameters, preconditions, effects, roles and risk. Every run is planned, audited and undoable."
      actions={
        <>
          <Button variant="primary" disabled={!canEdit || !!err || !jsonOk || d.effects.length === 0} onClick={() => onStage(n, clean({ ...d, name: n }))}>
            {isNew ? "Add to change set" : "Stage changes"}
          </Button>
          {!isNew && onDelete ? (
            <Button variant="danger" icon={<Trash2 className="size-4" />} disabled={!canEdit} onClick={() => onDelete(name!)}>
              Delete action type
            </Button>
          ) : null}
        </>
      }
    >
      <Grid>
        <Field label="Name" error={err}>
          {(id) => <Input id={id} className="font-mono" value={n} disabled={!isNew || !canEdit} onChange={(e) => setN(e.target.value)} />}
        </Field>
        <Field label="Risk" hint="Medium and high need confirmation">
          {(id) => (
            <Select id={id} value={d.risk} disabled={!canEdit} onChange={(e) => set({ risk: e.target.value as ActionType["risk"] })}>
              <option>low</option>
              <option>medium</option>
              <option>high</option>
            </Select>
          )}
        </Field>
      </Grid>
      <Field label="Description">{(id) => <Textarea id={id} value={d.description ?? ""} disabled={!canEdit} onChange={(e) => set({ description: e.target.value })} />}</Field>
      <Grid>
        <Field label="Roles" hint="Empty: anyone who can write">
          {() => <TagInput value={d.roles ?? []} suggestions={["member", "agent", "admin", "viewer"]} onChange={(roles) => set({ roles })} />}
        </Field>
        <Field label="Phrases" hint="Verbs that trigger it from a sentence">
          {() => <TagInput value={d.phrases ?? []} onChange={(phrases) => set({ phrases })} />}
        </Field>
      </Grid>
      <div>
        <p className="mb-2 text-[13px] font-medium">Parameters</p>
        <div className="flex flex-col gap-2">
          {params.map(([pname, p], i) => (
            <div key={i} className="rounded-card bg-raised p-3 shadow-card">
              <div className="grid grid-cols-[1fr_1fr_auto] gap-2">
                <Input aria-label="Parameter name" className="font-mono" defaultValue={pname} disabled={!canEdit} onBlur={(e) => e.target.value !== pname && setParam(pname, e.target.value, p)} />
                <Select aria-label="Parameter type" value={p.type} disabled={!canEdit} onChange={(e) => setParam(pname, pname, { ...p, type: e.target.value })}>
                  {PARAM_TYPES.map((t) => (
                    <option key={t}>{t}</option>
                  ))}
                </Select>
                <IconButton label={`Remove ${pname}`} disabled={!canEdit} onClick={() => setParam(pname, "", p)}>
                  <Trash2 className="size-4" />
                </IconButton>
              </div>
              {p.type === "entity" ? (
                <div className="mt-2">
                  <TagInput value={p.kinds ?? []} suggestions={kinds} placeholder="Allowed kinds" onChange={(k) => setParam(pname, pname, { ...p, kinds: k })} />
                </div>
              ) : null}
              {p.type === "enum" ? (
                <div className="mt-2">
                  <TagInput value={p.values ?? []} placeholder="Allowed values" onChange={(v) => setParam(pname, pname, { ...p, values: v })} />
                </div>
              ) : null}
              <div className="mt-2 flex flex-wrap items-center gap-4">
                <Checkbox label="Required" checked={!!p.required} disabled={!canEdit} onChange={(v) => setParam(pname, pname, { ...p, required: v })} />
                <Input aria-label="Description" placeholder="Description" className="min-w-40 flex-1" value={p.description ?? ""} disabled={!canEdit} onChange={(e) => setParam(pname, pname, { ...p, description: e.target.value })} />
              </div>
            </div>
          ))}
          <div>
            <Button
              size="sm"
              variant="ghost"
              icon={<Plus className="size-4" />}
              disabled={!canEdit}
              onClick={() => {
                let i = params.length + 1;
                while (d.parameters?.[`param_${i}`]) i++;
                set({ parameters: { ...(d.parameters ?? {}), [`param_${i}`]: { type: "string" } } });
              }}
            >
              Add parameter
            </Button>
          </div>
        </div>
      </div>
      <JsonField
        label="Preconditions"
        hint='Checks before anything runs, e.g. [{"check": "open", "param": "item"}]'
        value={d.preconditions ?? []}
        disabled={!canEdit}
        onChange={(v) => {
          setJsonOk(v !== null);
          if (v) set({ preconditions: v });
        }}
      />
      <JsonField
        label="Effects"
        hint='Each one of patch, link, unlink, create, delete, event; placeholders like {item} name parameters. e.g. [{"patch": {"id": "{item}", "state": "done"}}]'
        value={d.effects}
        disabled={!canEdit}
        onChange={(v) => {
          setJsonOk(v !== null);
          if (v) set({ effects: v });
        }}
      />
    </EditorFrame>
  );
}

// ── rules ──────────────────────────────────────────────────────────────

export function RuleEditor({ schema, name, initial, onStage, onDelete, canEdit }: EditorProps<Rule>) {
  const isNew = !name;
  const [d, setD] = useState<Rule>(() => initial ?? { name: "", chain: [], infer: "" });
  useEffect(() => setD(initial ?? { name: "", chain: [], infer: "" }), [initial]);
  const rels = useMemo(
    () => [...Object.keys(schema.link_types), ...Object.values(schema.link_types).map((l) => l.inverse ?? "").filter(Boolean)],
    [schema],
  );
  const err = nameError(d.name, schema.rules.map((r) => r.name), isNew);
  const inferErr = d.infer && !NAME_RE.test(d.infer) ? "Lowercase letters, digits and _" : undefined;
  return (
    <EditorFrame
      title={isNew ? "New rule" : name!}
      subtitle="Derives a relation from a chain: assigned_to › member_of ⇒ handled_by_team."
      actions={
        <>
          <Button variant="primary" disabled={!canEdit || !!err || !!inferErr || !d.infer || d.chain.length === 0} onClick={() => onStage(d.name, clean(d))}>
            {isNew ? "Add to change set" : "Stage changes"}
          </Button>
          {!isNew && onDelete ? (
            <Button variant="danger" icon={<Trash2 className="size-4" />} disabled={!canEdit} onClick={() => onDelete(name!)}>
              Delete rule
            </Button>
          ) : null}
        </>
      }
    >
      <Grid>
        <Field label="Name" error={err}>
          {(id) => <Input id={id} className="font-mono" value={d.name} disabled={!isNew || !canEdit} onChange={(e) => setD({ ...d, name: e.target.value })} />}
        </Field>
        <Field label="Infers" error={inferErr} hint="The derived relation's name">
          {(id) => <Input id={id} className="font-mono" value={d.infer} disabled={!canEdit} onChange={(e) => setD({ ...d, infer: e.target.value })} />}
        </Field>
      </Grid>
      <Field label="Chain" hint="Relations walked in order (inverse names and ^rel allowed)">
        {() => <TagInput value={d.chain} suggestions={rels} onChange={(chain) => setD({ ...d, chain })} />}
      </Field>
      <Field label="Description">{(id) => <Textarea id={id} value={d.description ?? ""} disabled={!canEdit} onChange={(e) => setD({ ...d, description: e.target.value })} />}</Field>
    </EditorFrame>
  );
}

// ── interfaces ─────────────────────────────────────────────────────────

export function InterfaceEditor({ schema, name, initial, onStage, onDelete, canEdit }: EditorProps<InterfaceType>) {
  const isNew = !name;
  const [n, setN] = useState(name ?? "");
  const [d, setD] = useState<InterfaceType>(() => initial ?? { name: "" });
  useEffect(() => {
    setN(name ?? "");
    setD(initial ?? { name: "" });
  }, [name, initial]);
  const err = nameError(n, [...Object.keys(schema.interfaces), ...Object.keys(schema.object_types)], isNew);
  return (
    <EditorFrame
      title={isNew ? "New interface" : name!}
      subtitle="A shape several object types share; queries for it match every implementer."
      actions={
        <>
          <Button variant="primary" disabled={!canEdit || !!err} onClick={() => onStage(n, clean({ ...d, name: n }))}>
            {isNew ? "Add to change set" : "Stage changes"}
          </Button>
          {!isNew && onDelete ? (
            <Button variant="danger" icon={<Trash2 className="size-4" />} disabled={!canEdit} onClick={() => onDelete(name!)}>
              Delete interface
            </Button>
          ) : null}
        </>
      }
    >
      <Field label="Name" error={err}>
        {(id) => <Input id={id} className="font-mono" value={n} disabled={!isNew || !canEdit} onChange={(e) => setN(e.target.value)} />}
      </Field>
      <Field label="Description">{(id) => <Textarea id={id} value={d.description ?? ""} disabled={!canEdit} onChange={(e) => setD({ ...d, description: e.target.value })} />}</Field>
      <Field label="Required relations" hint="Every implementer must be allowed to use them">
        {() => <TagInput value={d.links ?? []} suggestions={Object.keys(schema.link_types)} onChange={(links) => setD({ ...d, links })} />}
      </Field>
      <div>
        <p className="mb-2 text-[13px] font-medium">Shared properties</p>
        <PropertyTable value={d.properties ?? {}} disabled={!canEdit} onChange={(properties) => setD({ ...d, properties })} />
      </div>
    </EditorFrame>
  );
}
