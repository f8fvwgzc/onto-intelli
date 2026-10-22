import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CircleCheck, CircleX, PlugZap, TriangleAlert } from "lucide-react";

import { Card, ConfirmDialog, Page, RoleButton } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { Button, ErrorState, Field, Input, Select, Skeleton } from "../../components/ui";
import { errorText } from "../../lib/api";
import { atLeast, consoleApi, needs, qk, useConsoleScope, type LlmSettings, type LlmTest, type LlmUpdate } from "../../lib/console";
import { ms, num, when } from "../../lib/format";
import { useWhoami } from "../../lib/queries";

const PROVIDERS = [
  { value: "anthropic", label: "Anthropic", key: true },
  { value: "openai", label: "OpenAI", key: true },
  { value: "openai-compatible", label: "OpenAI-compatible (base URL)", key: false },
  { value: "ollama", label: "Ollama (base URL)", key: false },
  { value: "none", label: "None: deterministic answers only", key: false },
];

interface Form {
  provider: string;
  model: string;
  base_url: string;
  api_key: string;
  replacing: boolean;
  removing: boolean;
  max_output_tokens: string;
  temperature: string;
  effort: string;
  timeout_secs: string;
  input_per_mtok: string;
  output_per_mtok: string;
  cached_input_per_mtok: string;
  monthly_budget_usd: string;
  monthly_token_budget: string;
  asks_per_minute: string;
}

const str = (v: number | null | undefined) => (v === null || v === undefined ? "" : String(v));

function formOf(s: LlmSettings | null): Form {
  return {
    provider: s?.provider ?? "anthropic",
    model: s?.model ?? "",
    base_url: s?.base_url ?? "",
    api_key: "",
    replacing: !s?.has_api_key,
    removing: false,
    max_output_tokens: str(s?.max_output_tokens ?? 4096),
    temperature: str(s?.temperature),
    effort: s?.effort ?? "",
    timeout_secs: str(s?.timeout_secs ?? 120),
    input_per_mtok: s?.pricing.input_per_mtok ? String(s.pricing.input_per_mtok) : "",
    output_per_mtok: s?.pricing.output_per_mtok ? String(s.pricing.output_per_mtok) : "",
    cached_input_per_mtok: s?.pricing.cached_input_per_mtok ? String(s.pricing.cached_input_per_mtok) : "",
    monthly_budget_usd: str(s?.monthly_budget_usd),
    monthly_token_budget: str(s?.monthly_token_budget),
    asks_per_minute: str(s?.asks_per_minute),
  };
}

function numberOr(v: string): number | undefined {
  const t = v.trim();
  if (!t) return undefined;
  const n = Number(t);
  return Number.isFinite(n) ? n : NaN;
}

/** The stored key belongs to another provider or base URL than the form's. */
function keyMoved(f: Pick<Form, "provider" | "base_url">, stored: LlmSettings | null): boolean {
  return Boolean(stored?.has_api_key) && (stored?.provider !== f.provider || (stored?.base_url ?? "") !== f.base_url.trim().replace(/\/+$/, ""));
}

/**
 * The body of `PUT …/llm` (or a test draft). Extra headers are kept as
 * stored, except masked ones (`***`: credential headers the server no longer
 * accepts), which are dropped.
 */
function updateOf(f: Form, stored: LlmSettings | null): LlmUpdate {
  const headers = Object.fromEntries(Object.entries(stored?.headers ?? {}).filter(([, v]) => v !== "***"));
  const u: LlmUpdate = { provider: f.provider, model: f.model.trim(), base_url: f.base_url.trim() || null, headers };
  if (f.removing) u.api_key = "";
  else if (f.api_key.trim()) u.api_key = f.api_key.trim();
  // The stored key is never sent to a new provider or base URL: with no new
  // key typed, the new target has none (keyless providers need none;
  // Anthropic and OpenAI get the server's "needs an api_key").
  else if (keyMoved(f, stored)) u.api_key = "";
  const n = (k: keyof Form) => {
    const v = numberOr(f[k] as string);
    if (Number.isNaN(v)) throw new Error(`${k.replace(/_/g, " ")} must be a number`);
    return v;
  };
  const set = <K extends keyof LlmUpdate>(k: K, v: LlmUpdate[K] | undefined) => {
    if (v !== undefined) u[k] = v;
  };
  set("max_output_tokens", n("max_output_tokens"));
  set("temperature", n("temperature"));
  set("effort", f.effort || undefined);
  set("timeout_secs", n("timeout_secs"));
  const pr = [n("input_per_mtok"), n("output_per_mtok"), n("cached_input_per_mtok")];
  if (pr.some((p) => p !== undefined)) u.pricing = { input_per_mtok: pr[0] ?? 0, output_per_mtok: pr[1] ?? 0, cached_input_per_mtok: pr[2] ?? 0 };
  set("monthly_budget_usd", n("monthly_budget_usd"));
  set("monthly_token_budget", n("monthly_token_budget"));
  set("asks_per_minute", n("asks_per_minute"));
  return u;
}

function TestResult({ r }: { r: LlmTest }) {
  return (
    <div role="status" className={r.ok ? "flex gap-2 rounded-card bg-accent-tint p-3 text-[13px]" : "flex gap-2 rounded-card bg-danger-tint p-3 text-[13px]"}>
      {r.ok ? <CircleCheck className="mt-0.5 size-4 shrink-0 text-success" aria-hidden /> : <CircleX className="mt-0.5 size-4 shrink-0 text-danger" aria-hidden />}
      <div className="min-w-0">
        <p className="font-medium text-ink">
          {r.ok ? "Connected" : "The provider did not answer"} · {r.provider}
          {r.response_model || r.model ? ` · ${r.response_model ?? r.model}` : ""} · {ms(r.latency_ms)}
        </p>
        {r.error ? <p className="mt-0.5 break-words text-ink-2">{r.error}</p> : null}
        {r.usage ? (
          <p className="mt-0.5 text-xs text-ink-3">
            {num(r.usage.input_tokens ?? 0)} input and {num(r.usage.output_tokens ?? 0)} output tokens; tests are not metered.
          </p>
        ) : null}
      </div>
    </div>
  );
}

export default function LlmPage() {
  const qc = useQueryClient();
  const { project, role } = useConsoleScope();
  const dev = useWhoami().data?.dev_mode ?? false;
  const ref = project?.ref ?? "";
  const q = useQuery({ queryKey: qk.llm(ref), queryFn: () => consoleApi.llm(ref), enabled: Boolean(project) });
  const stored = q.data?.llm ?? null;
  const [f, setF] = useState<Form>(() => formOf(null));
  const [reset, setReset] = useState(false);
  const [tested, setTested] = useState<LlmTest>();
  useEffect(() => {
    if (q.data) setF(formOf(q.data.llm));
  }, [q.data]);
  const admin = atLeast(role, "admin");
  const set = (patch: Partial<Form>) => setF((cur) => ({ ...cur, ...patch }));

  const save = useMutation({
    mutationFn: () => consoleApi.putLlm(ref, updateOf(f, stored)),
    onSuccess: (v) => {
      qc.setQueryData(qk.llm(ref), v);
      void qc.invalidateQueries({ queryKey: ["console", "usage", ref] });
    },
  });
  const test = useMutation({ mutationFn: () => consoleApi.testLlm(ref, updateOf(f, stored)), onSuccess: setTested, onMutate: () => setTested(undefined) });
  const remove = useMutation({
    mutationFn: () => consoleApi.deleteLlm(ref),
    onSuccess: () => {
      setReset(false);
      void qc.invalidateQueries({ queryKey: qk.llm(ref) });
    },
  });

  const keyed = PROVIDERS.find((p) => p.value === f.provider)?.key ?? false;
  const needsBase = f.provider === "openai-compatible" || f.provider === "ollama";
  const moved = keyMoved(f, stored);
  const showKeyInput = f.replacing || moved;
  const sd = q.data?.server_default;

  return (
    <>
      <PageHeader title="LLM" context="This project's own model provider, prices, budgets and rate limit">
        {stored ? (
          <RoleButton size="sm" variant="ghost" role={role} min="admin" onClick={() => setReset(true)}>
            Use server default
          </RoleButton>
        ) : null}
      </PageHeader>
      <Page>
        {q.error ? (
          <ErrorState error={q.error} retry={() => void q.refetch()} />
        ) : !q.data ? (
          <Skeleton className="h-96 rounded-card" />
        ) : (
          <form
            className="mx-auto flex max-w-3xl flex-col gap-3"
            onSubmit={(e) => {
              e.preventDefault();
              if (admin) save.mutate();
            }}
          >
            <p className="text-[13px] text-ink-2">
              {stored ? (
                <>
                  Using this project's {stored.provider} settings, changed {when(stored.updated_at)}.
                </>
              ) : (
                <>
                  Using the server default: <span className="font-medium text-ink">{sd?.provider}</span>
                  {sd?.model ? ` · ${sd.model}` : ""}. Save settings below to bring your own account.
                </>
              )}
            </p>
            {q.data.error ? (
              <p role="alert" className="flex gap-2 rounded-card bg-danger-tint p-3 text-[13px] text-danger">
                <TriangleAlert className="mt-0.5 size-4 shrink-0" aria-hidden /> {q.data.error}. The server default is in use until this is fixed.
              </p>
            ) : null}
            {!q.data.master_key ? (
              <p className="flex gap-2 rounded-card bg-warning-tint p-3 text-[13px] text-ink">
                <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden /> ONTO_MASTER_KEY is not set on the server, so API keys cannot be stored. Ollama,
                OpenAI-compatible servers without a key and “None” still work.
              </p>
            ) : null}
            <fieldset disabled={!admin} className="contents">
              <Card title="Provider" aside={!admin ? <span className="text-xs text-ink-3">{needs("admin")} to change</span> : null}>
                <div className="grid gap-4 sm:grid-cols-2">
                  <Field label="Provider">
                    {(id) => (
                      <Select id={id} value={f.provider} onChange={(e) => set({ provider: e.target.value })}>
                        {PROVIDERS.map((p) => (
                          <option key={p.value} value={p.value}>
                            {p.label}
                          </option>
                        ))}
                        {dev ? <option value="mock">Mock (dev mode)</option> : null}
                      </Select>
                    )}
                  </Field>
                  <Field label="Model" hint="Empty: the provider's default">
                    {(id) => (
                      <Input
                        id={id}
                        value={f.model}
                        onChange={(e) => set({ model: e.target.value })}
                        maxLength={200}
                        placeholder={f.provider === "anthropic" ? "claude-…" : f.provider === "openai" ? "gpt-…" : ""}
                      />
                    )}
                  </Field>
                  <div className="sm:col-span-2">
                    <Field label={needsBase ? "Base URL" : "Base URL (optional proxy)"} hint="https://…, no credentials or query">
                      {(id) => (
                        <Input
                          id={id}
                          value={f.base_url}
                          onChange={(e) => set({ base_url: e.target.value })}
                          required={needsBase}
                          placeholder={f.provider === "ollama" ? "https://ollama.internal:11434" : "https://"}
                          inputMode="url"
                        />
                      )}
                    </Field>
                  </div>
                  <div className="sm:col-span-2">
                    <Field
                      label="API key"
                      hint={
                        moved && !f.api_key
                          ? "A stored key stays with the provider and base URL it was entered for: enter this one's key, or leave it empty if it needs none."
                          : "Write-only: stored encrypted, never shown again."
                      }
                    >
                      {(id) =>
                        stored?.has_api_key && !showKeyInput && !f.removing ? (
                          <div className="flex items-center gap-2">
                            <span id={id} className="tnum flex h-9 flex-1 items-center rounded-input bg-sunken px-2.5 font-mono text-sm text-ink-2">
                              •••• {stored.api_key_last4 ?? ""}
                            </span>
                            <Button type="button" size="sm" onClick={() => set({ replacing: true })}>
                              Replace
                            </Button>
                            <Button type="button" size="sm" variant="ghost" onClick={() => set({ removing: true })}>
                              Remove
                            </Button>
                          </div>
                        ) : f.removing ? (
                          <div className="flex items-center gap-2 text-[13px] text-ink-2">
                            <span className="flex-1">The stored key is removed when you save.</span>
                            <Button type="button" size="sm" variant="ghost" onClick={() => set({ removing: false })}>
                              Keep it
                            </Button>
                          </div>
                        ) : (
                          <div className="flex items-center gap-2">
                            <Input
                              id={id}
                              type="password"
                              autoComplete="off"
                              value={f.api_key}
                              onChange={(e) => set({ api_key: e.target.value })}
                              placeholder={keyed ? "Required" : "Optional"}
                              disabled={!q.data.master_key}
                            />
                            {stored?.has_api_key && !moved ? (
                              <Button type="button" size="sm" variant="ghost" onClick={() => set({ replacing: false, api_key: "" })}>
                                Cancel
                              </Button>
                            ) : null}
                          </div>
                        )
                      }
                    </Field>
                  </div>
                </div>
              </Card>
              <Card title="Requests">
                <div className="grid gap-4 sm:grid-cols-4">
                  <Field label="Max output tokens">
                    {(id) => <Input id={id} inputMode="numeric" value={f.max_output_tokens} onChange={(e) => set({ max_output_tokens: e.target.value })} />}
                  </Field>
                  <Field label="Temperature" hint="0 to 2">
                    {(id) => <Input id={id} inputMode="decimal" value={f.temperature} onChange={(e) => set({ temperature: e.target.value })} placeholder="Default" />}
                  </Field>
                  <Field label="Effort">
                    {(id) => (
                      <Select id={id} value={f.effort} onChange={(e) => set({ effort: e.target.value })}>
                        <option value="">Default</option>
                        {["low", "medium", "high", "xhigh", "max"].map((x) => (
                          <option key={x} value={x}>
                            {x}
                          </option>
                        ))}
                      </Select>
                    )}
                  </Field>
                  <Field label="Timeout (s)">{(id) => <Input id={id} inputMode="numeric" value={f.timeout_secs} onChange={(e) => set({ timeout_secs: e.target.value })} />}</Field>
                </div>
                {stored && Object.keys(stored.headers).length ? (
                  <p className="mt-3 text-xs text-ink-3">Extra request headers (kept): {Object.keys(stored.headers).join(", ")}</p>
                ) : null}
              </Card>
              <Card title="Pricing" aside={<span className="text-xs text-ink-3">USD per million tokens; empty uses list prices where known</span>}>
                <div className="grid gap-4 sm:grid-cols-3">
                  <Field label="Input">
                    {(id) => <Input id={id} inputMode="decimal" value={f.input_per_mtok} onChange={(e) => set({ input_per_mtok: e.target.value })} placeholder="List price" />}
                  </Field>
                  <Field label="Output">
                    {(id) => <Input id={id} inputMode="decimal" value={f.output_per_mtok} onChange={(e) => set({ output_per_mtok: e.target.value })} placeholder="List price" />}
                  </Field>
                  <Field label="Cached input">
                    {(id) => (
                      <Input
                        id={id}
                        inputMode="decimal"
                        value={f.cached_input_per_mtok}
                        onChange={(e) => set({ cached_input_per_mtok: e.target.value })}
                        placeholder="List price"
                      />
                    )}
                  </Field>
                </div>
              </Card>
              <Card title="Budgets and rate limit" aside={<span className="text-xs text-ink-3">Per calendar month, UTC; at the budget, LLM steps are skipped</span>}>
                <div className="grid gap-4 sm:grid-cols-3">
                  <Field label="Monthly budget (USD)">
                    {(id) => (
                      <Input id={id} inputMode="decimal" value={f.monthly_budget_usd} onChange={(e) => set({ monthly_budget_usd: e.target.value })} placeholder="No budget" />
                    )}
                  </Field>
                  <Field label="Monthly tokens" hint="Input plus output">
                    {(id) => (
                      <Input id={id} inputMode="numeric" value={f.monthly_token_budget} onChange={(e) => set({ monthly_token_budget: e.target.value })} placeholder="No budget" />
                    )}
                  </Field>
                  <Field label="Asks per minute" hint="Per server replica">
                    {(id) => <Input id={id} inputMode="numeric" value={f.asks_per_minute} onChange={(e) => set({ asks_per_minute: e.target.value })} placeholder="No limit" />}
                  </Field>
                </div>
              </Card>
            </fieldset>
            {tested ? <TestResult r={tested} /> : null}
            {test.error ? <p className="text-[13px] text-danger">{errorText(test.error)}</p> : null}
            {save.error ? <p className="text-[13px] text-danger">{errorText(save.error)}</p> : null}
            {save.isSuccess && !save.isPending ? (
              <p className="text-[13px] text-ink-2" role="status">
                Saved. New questions use these settings.
              </p>
            ) : null}
            <div className="flex flex-wrap items-center justify-end gap-2">
              <RoleButton type="button" role={role} min="developer" busy={test.isPending} icon={<PlugZap className="size-4" aria-hidden />} onClick={() => test.mutate()}>
                Test connection
              </RoleButton>
              <RoleButton type="submit" variant="primary" role={role} min="admin" busy={save.isPending}>
                Save settings
              </RoleButton>
            </div>
          </form>
        )}
      </Page>
      <ConfirmDialog
        open={reset}
        onClose={() => setReset(false)}
        title="Use the server default"
        confirm="Remove project settings"
        busy={remove.isPending}
        error={remove.error ? errorText(remove.error) : undefined}
        onConfirm={() => remove.mutate()}
      >
        <p>The project's provider, stored key, prices, budgets and rate limit are removed; questions use the server's {sd?.provider ?? "default"} provider again.</p>
      </ConfirmDialog>
    </>
  );
}
