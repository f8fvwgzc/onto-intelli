import { useEffect, useId, useMemo, useState } from "react";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CircleAlert, CircleCheck } from "lucide-react";

import { call, errorText } from "../lib/api";
import { kindLabel } from "../lib/format";
import { useSchema, useScope } from "../lib/queries";
import type { ActionPlan, ActionReceipt, ActionType, Entity, Json, ParamDef } from "../lib/types";
import { Badge, Button, Checkbox, Dialog, Field, Input, Select, Spinner, Textarea, cx } from "./ui";

/** The action types that take an entity of `kind` (or create one). */
export function actionsFor(kind: string, types: Record<string, ActionType> | undefined): { acting: string[]; creating: string[] } {
  const acting: string[] = [];
  const creating: string[] = [];
  for (const [name, at] of Object.entries(types ?? {})) {
    if (Object.values(at.parameters ?? {}).some((p) => p.type === "entity" && (p.kinds ?? []).includes(kind))) acting.push(name);
    if ((at.effects ?? []).some((e) => typeof e === "object" && e !== null && !Array.isArray(e) && (e.create as { kind?: string } | undefined)?.kind === kind)) creating.push(name);
  }
  return { acting: acting.sort(), creating: creating.sort() };
}

const RISK_TONE = { low: "neutral", medium: "warning", high: "danger" } as const;

/**
 * Run an action type the governed way: fill its parameters, preview the
 * plan (`action.plan`: checks, edits, risk), then apply it (`action.apply`).
 * `item` fills the parameter that takes the item's kind.
 */
export function ActionDialog({ actions, item, kind, workspace, onClose }: { actions: string[]; item?: Entity; kind: string; workspace?: string; onClose: () => void }) {
  const schema = useSchema().data;
  const qc = useQueryClient();
  const [action, setAction] = useState(actions[0] ?? "");
  const at = schema?.action_types?.[action];
  const [values, setValues] = useState<Record<string, string | boolean>>({});
  const [confirm, setConfirm] = useState(false);
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);

  // The parameter the item fills, and a fresh form for each action type.
  const itemParam = useMemo(
    () => (item && at ? Object.entries(at.parameters ?? {}).find(([, p]) => p.type === "entity" && (p.kinds ?? []).includes(item.kind))?.[0] : undefined),
    [at, item],
  );
  useEffect(() => {
    setValues(itemParam && item ? { [itemParam]: item.id } : {});
    setConfirm(false);
    setReceipt(null);
  }, [action, itemParam, item]);

  const params = useMemo(() => toParams(at?.parameters ?? {}, values), [at, values]);
  const ready = !!at && params.missing.length === 0 && params.errors.length === 0;
  const plan = useQuery<ActionPlan>({
    queryKey: ["action-plan", action, JSON.stringify(params.values), workspace],
    queryFn: () => call<ActionPlan>("action.plan", { action, params: params.values, workspace }),
    enabled: ready && !receipt,
    placeholderData: keepPreviousData,
  });
  const apply = useMutation({
    mutationFn: () => call<ActionReceipt>("action.apply", { action, params: params.values, workspace, confirm }),
    onSuccess: (r) => {
      setReceipt(r);
      if (r.status === "applied") {
        void qc.invalidateQueries({ queryKey: ["explore"] });
        void qc.invalidateQueries({ queryKey: ["object-types"] });
        void qc.invalidateQueries({ queryKey: ["entities"] });
        void qc.invalidateQueries({ queryKey: ["entity"] });
        void qc.invalidateQueries({ queryKey: ["overview"] });
      }
    },
  });
  const p = plan.data;
  const needsConfirm = !!p?.needs_confirmation;

  return (
    <Dialog
      open
      onClose={onClose}
      title={item ? `Act on ${item.title || item.id}` : `New ${kindLabel(kind, schema).toLowerCase()}`}
      footer={
        receipt?.status === "applied" ? (
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        ) : (
          <>
            {apply.error ? <p className="mr-auto self-center text-[13px] text-danger">{errorText(apply.error)}</p> : null}
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button
              variant="primary"
              busy={apply.isPending}
              disabled={!ready || !p?.valid || plan.isFetching || (needsConfirm && !confirm)}
              onClick={() => apply.mutate()}
            >
              Apply
            </Button>
          </>
        )
      }
    >
      <div className="flex flex-col gap-4">
        {actions.length > 1 ? (
          <Field label="Action">
            {(id) => (
              <Select id={id} value={action} onChange={(e) => setAction(e.target.value)}>
                {actions.map((a) => (
                  <option key={a} value={a}>
                    {a}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        ) : (
          <p className="font-mono text-[13px] text-ink">{action}</p>
        )}
        {at?.description ? <p className="-mt-2 text-[13px] text-ink-2">{at.description}</p> : null}
        {!at ? <p className="text-[13px] text-ink-3">This action type is not in the schema.</p> : null}

        {at
          ? Object.entries(at.parameters ?? {}).map(([name, def]) => (
              <ParamInput
                key={`${action}.${name}`}
                name={name}
                def={def}
                value={values[name]}
                fixed={name === itemParam}
                onChange={(v) => setValues((s) => ({ ...s, [name]: v }))}
              />
            ))
          : null}
        {params.errors.map((e) => (
          <p key={e} className="text-xs text-danger">
            {e}
          </p>
        ))}

        <section aria-live="polite" className="rounded-card bg-sunken p-3">
          {receipt ? (
            <ReceiptView receipt={receipt} />
          ) : !ready ? (
            <p className="text-[13px] text-ink-3">{params.missing.length ? `Fill in ${params.missing.join(", ")} to preview the edits.` : "Fix the values above to preview the edits."}</p>
          ) : plan.isLoading ? (
            <Spinner label="Checking" />
          ) : plan.error ? (
            <p className="text-[13px] text-danger">{errorText(plan.error)}</p>
          ) : p ? (
            <PlanSummary plan={p} />
          ) : null}
        </section>
        {needsConfirm && !receipt ? (
          <Checkbox label={`I understand: risk ${p!.risk}${p!.reversible ? "" : ", not reversible"}`} checked={confirm} onChange={setConfirm} />
        ) : null}
      </div>
    </Dialog>
  );
}

function PlanSummary({ plan }: { plan: ActionPlan }) {
  return (
    <div className="flex flex-col gap-2 text-[13px]">
      <div className="flex flex-wrap items-center gap-2">
        {plan.valid ? <Badge tone="accent">Ready</Badge> : <Badge tone="danger">Blocked</Badge>}
        <Badge tone={RISK_TONE[plan.risk]}>risk {plan.risk}</Badge>
        <span className="text-xs text-ink-3">{plan.reversible ? "Reversible from its receipt" : "Not reversible"}</span>
      </div>
      {plan.checks.length ? (
        <ul className="flex flex-col gap-1">
          {plan.checks.map((c, i) => (
            <li key={i} className="flex items-start gap-1.5">
              {c.ok ? <CircleCheck className="mt-0.5 size-3.5 shrink-0 text-success" aria-label="Passes" /> : <CircleAlert className="mt-0.5 size-3.5 shrink-0 text-danger" aria-label="Fails" />}
              {/* A check's message describes its failure, so a passing one shows its name. */}
              <span className={c.ok ? "text-ink-2" : "text-ink"}>{c.ok ? `${c.check.replace(/_/g, " ")} passes` : c.message || c.check}</span>
            </li>
          ))}
        </ul>
      ) : null}
      {plan.preview.length ? (
        <ul className="flex list-disc flex-col gap-0.5 pl-5 text-ink">
          {plan.preview.map((l, i) => (
            <li key={i}>{l}</li>
          ))}
        </ul>
      ) : null}
      {plan.warnings.map((w) => (
        <p key={w} className="text-xs text-ink-2">
          {w}
        </p>
      ))}
    </div>
  );
}

function ReceiptView({ receipt }: { receipt: ActionReceipt }) {
  const ok = receipt.status === "applied";
  return (
    <div className="text-[13px]">
      <p className={cx("flex items-center gap-1.5 font-medium", ok ? "text-ink" : "text-danger")}>
        {ok ? <CircleCheck className="size-4 text-success" aria-hidden /> : <CircleAlert className="size-4" aria-hidden />}
        {ok ? "Applied" : receipt.status === "rejected" ? "Not applied" : "Waiting"}
      </p>
      {receipt.reason ? <p className="mt-1 text-ink-2">{receipt.reason}</p> : null}
      <p className="mt-1 font-mono text-xs text-ink-3">receipt {receipt.id}</p>
    </div>
  );
}

function ParamInput({ name, def, value, fixed, onChange }: { name: string; def: ParamDef; value: string | boolean | undefined; fixed: boolean; onChange: (v: string | boolean) => void }) {
  const label = `${name}${def.required ? "" : " (optional)"}`;
  const hint = def.description && def.description !== name ? def.description : undefined;
  if (def.type === "bool") return <Checkbox label={hint ? `${name}: ${hint}` : name} checked={value === true} onChange={onChange} />;
  return (
    <Field label={label} hint={def.type === "entity" ? [hint, (def.kinds ?? []).join(", ")].filter(Boolean).join(" · ") : hint}>
      {(id) =>
        def.type === "enum" ? (
          <Select id={id} value={typeof value === "string" ? value : ""} onChange={(e) => onChange(e.target.value)}>
            <option value="">{def.required ? "Choose…" : "Leave as is"}</option>
            {(def.values ?? []).map((v) => (
              <option key={v} value={v}>
                {v}
              </option>
            ))}
          </Select>
        ) : def.type === "text" ? (
          <Textarea id={id} value={typeof value === "string" ? value : ""} onChange={(e) => onChange(e.target.value)} />
        ) : def.type === "entity" ? (
          <EntityPicker id={id} kinds={def.kinds ?? []} value={typeof value === "string" ? value : ""} disabled={fixed} onChange={onChange} />
        ) : (
          <Input
            id={id}
            inputMode={def.type === "int" ? "numeric" : def.type === "float" ? "decimal" : undefined}
            value={typeof value === "string" ? value : ""}
            onChange={(e) => onChange(e.target.value)}
          />
        )
      }
    </Field>
  );
}

/** An entity id, with suggestions from search as one types. */
function EntityPicker({ id, kinds, value, disabled, onChange }: { id: string; kinds: string[]; value: string; disabled: boolean; onChange: (v: string) => void }) {
  const listId = useId();
  const scope = useScope();
  const [debounced, setDebounced] = useState("");
  useEffect(() => {
    const t = window.setTimeout(() => setDebounced(value.trim()), 180);
    return () => window.clearTimeout(t);
  }, [value]);
  const hits = useQuery<{ hits: { entity: Entity }[] }>({
    queryKey: ["picker", kinds.join(","), debounced, ...scope],
    queryFn: () => call("entity.search", { query: debounced, kinds: kinds.length ? kinds : undefined, limit: 8 }),
    enabled: !disabled && debounced.length > 1 && !debounced.includes(":"),
  });
  return (
    <>
      <Input id={id} list={listId} value={value} disabled={disabled} placeholder={kinds.length ? `${kinds[0]}:… or search` : "id"} className="font-mono text-[13px]" onChange={(e) => onChange(e.target.value)} />
      <datalist id={listId}>
        {(hits.data?.hits ?? []).map(({ entity }) => (
          <option key={entity.id} value={entity.id}>
            {entity.title}
          </option>
        ))}
      </datalist>
    </>
  );
}

/** Form values → action parameters; empty optional values are left out. */
function toParams(defs: Record<string, ParamDef>, values: Record<string, string | boolean>): { values: Record<string, Json>; missing: string[]; errors: string[] } {
  const out: Record<string, Json> = {};
  const missing: string[] = [];
  const errors: string[] = [];
  for (const [name, def] of Object.entries(defs)) {
    const v = values[name];
    if (def.type === "bool") {
      if (v === true || def.required) out[name] = v === true;
      continue;
    }
    const s = typeof v === "string" ? v.trim() : "";
    if (!s) {
      if (def.required && def.default === undefined) missing.push(name);
      continue;
    }
    if (def.type === "int" || def.type === "float") {
      const n = Number(s);
      if (!Number.isFinite(n) || (def.type === "int" && !Number.isInteger(n))) errors.push(`${name} must be ${def.type === "int" ? "a whole number" : "a number"}`);
      else out[name] = n;
    } else out[name] = s;
  }
  return { values: out, missing, errors };
}
