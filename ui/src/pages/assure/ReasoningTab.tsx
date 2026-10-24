import { useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { BadgeCheck, Play, Search } from "lucide-react";

import { BarList } from "../../components/charts";
import { Card, Page, Stat, td, th } from "../../components/consoleUi";
import { DerivedFact, FactChip } from "../../components/Facts";
import { Badge, Button, ErrorState, Input, Skeleton, Textarea } from "../../components/ui";
import { call, errorText } from "../../lib/api";
import { num, plural } from "../../lib/format";
import { useScope } from "../../lib/queries";
import type { CertManifest, Certified, Justifications, QueryAnswer, ReasonRun } from "../../lib/types";
import { DownloadButton, Means, NotDone, Verdict, looseValue } from "./common";

const EXAMPLE = '[["?a", "depends_on", "?b"]]';

export function ReasoningTab() {
  const scope = useScope();
  const run = useQuery<ReasonRun>({ queryKey: ["reason", "run", ...scope], queryFn: () => call<ReasonRun>("reason.run", { samples: 20 }) });
  const r = run.data;
  const byRule = useMemo(
    () =>
      Object.entries(r?.by_rule ?? {})
        .filter(([, n]) => n > 0)
        .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
        .map(([label, value]) => ({ label, value, hint: label.startsWith("builtin/") ? "built-in" : "supplied" })),
    [r],
  );

  return (
    <Page>
      {run.error ? <ErrorState error={run.error} retry={() => void run.refetch()} /> : null}
      {run.isLoading ? (
        <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
          {[0, 1, 2, 3].map((i) => (
            <Skeleton key={i} className="h-[88px]" />
          ))}
        </div>
      ) : r ? (
        <>
          <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
            <Stat label="Asserted facts" value={num(r.asserted)} sub="stated by graph and schema" />
            <Stat label="Derived facts" value={num(r.derived)} sub="by rules, never stored" primary />
            <Stat label="Rounds" value={num(r.rounds)} sub={`${plural(r.rules, "rule")} in the table`} />
            <Stat
              label="Fixpoint"
              value={r.fixpoint_reached ? "reached" : "not reached"}
              sub={r.fixpoint_reached ? "nothing more follows" : (r.truncated ?? "a lower bound")}
              tone={r.fixpoint_reached ? "flat" : "bad"}
            />
          </div>

          <div className="mt-4">
            <NotDone title="Not evaluated by this run" items={r.not_evaluated}>
              A clean run is not a consistency result. The reasoner derives facts; it does not check these constructs, so their silence here means nothing.
            </NotDone>
          </div>
          {r.refused ? (
            <div className="mt-3 rounded-card bg-danger-tint p-3 text-[13px] text-ink">
              <p className="font-medium text-danger">{plural(r.refused, "term")} refused</p>
              <p className="mt-0.5 text-ink-2">They cannot be spelled safely, so no certificate can be written until they are fixed.</p>
              <ul className="mt-1.5 flex flex-col gap-0.5 font-mono text-xs text-ink-2">
                {(r.refusals ?? []).map((x) => (
                  <li key={x.term}>
                    {x.term}: {x.reason}
                  </li>
                ))}
              </ul>
            </div>
          ) : null}

          <div className="mt-4 grid gap-3 lg:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
            <Card title="Derived facts by rule">
              <BarList rows={byRule} empty="No rule derived anything" />
            </Card>
            <Card title="Sample derivations" aside={<span className="text-xs text-ink-3">dashed: derived</span>}>
              {r.samples?.length ? (
                <ul className="flex flex-col gap-2">
                  {r.samples.map((s, i) => (
                    <DerivedFact key={i} item={s} />
                  ))}
                </ul>
              ) : (
                <p className="text-[13px] text-ink-3">Nothing derived.</p>
              )}
            </Card>
          </div>
          <Means>{r.means}</Means>
        </>
      ) : null}

      <div className="mt-6 grid gap-3 xl:grid-cols-2">
        <QueryCard />
        <ExplainCard />
      </div>
      <div className="mt-3">
        <CertifyCard />
      </div>
    </Page>
  );
}

function QueryCard() {
  const [text, setText] = useState(EXAMPLE);
  const [parseError, setParseError] = useState<string>();
  const q = useMutation({ mutationFn: (pattern: unknown) => call<QueryAnswer>("reason.query", { pattern, limit: 50 }) });
  const submit = () => {
    let pattern: unknown;
    try {
      pattern = JSON.parse(text);
    } catch (e) {
      setParseError(e instanceof Error ? e.message : String(e));
      return;
    }
    if (!Array.isArray(pattern)) return setParseError("A pattern is a list of [subject, relation, object] atoms.");
    setParseError(undefined);
    q.mutate(pattern);
  };
  // Columns in the order the pattern names its variables.
  const vars = useMemo(() => {
    const named = JSON.stringify(q.variables ?? []).match(/"\?[^"]+"/g) ?? [];
    const order = named.map((v) => v.slice(2, -1));
    const rows = q.data?.bindings ?? [];
    return [...new Set([...order, ...rows.flatMap((b) => Object.keys(b))])].filter((v) => rows.some((b) => v in b));
  }, [q.data, q.variables]);

  return (
    <Card title="Query the closure" aside={<span className="text-xs text-ink-3">asserted and derived facts</span>}>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <label className="text-xs text-ink-3" htmlFor="reason-pattern">
          Atoms like rule <span className="font-mono">when</span> atoms; <span className="font-mono">?x</span> is a variable
        </label>
        <Textarea
          id="reason-pattern"
          value={text}
          onChange={(e) => setText(e.target.value)}
          spellCheck={false}
          className="mt-1 min-h-16 font-mono text-[12.5px]"
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              submit();
            }
          }}
        />
        <div className="mt-2 flex items-center gap-2">
          <Button size="sm" type="submit" icon={<Search className="size-4" aria-hidden />} busy={q.isPending}>
            Run query
          </Button>
          {parseError || q.error ? <p className="min-w-0 truncate text-[13px] text-danger">{parseError ?? errorText(q.error)}</p> : null}
        </div>
      </form>
      {q.data ? (
        <div className="mt-3">
          <p className="tnum text-xs text-ink-3">
            {plural(q.data.bindings.length, "answer")}
            {q.data.truncated ? ` · ${q.data.truncated}` : ""}
            {q.data.fixpoint_reached ? "" : " · the closure is a lower bound: more answers may exist"}
          </p>
          {q.data.bindings.length ? (
            <div className="mt-1.5 max-h-80 overflow-auto rounded-input shadow-card">
              <table className="w-full text-[12.5px]">
                <thead className="sticky top-0 bg-raised">
                  <tr className="border-b border-hairline">
                    {vars.map((v) => (
                      <th key={v} className={th}>
                        <span className="font-mono">?{v}</span>
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody className="divide-y divide-hairline">
                  {q.data.bindings.map((b, i) => (
                    <tr key={i}>
                      {vars.map((v) => (
                        <td key={v} className={`${td} py-1.5 font-mono break-all text-ink`}>
                          {b[v]}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : null}
        </div>
      ) : null}
    </Card>
  );
}

function ExplainCard() {
  const [s, setS] = useState("");
  const [p, setP] = useState("");
  const [o, setO] = useState("");
  const ex = useMutation({
    mutationFn: () => call<Justifications>("reason.explain", { fact: { subject: s.trim(), relation: p.trim(), object: looseValue(o) } }),
  });
  const j = ex.data;
  return (
    <Card title="Explain a fact" aside={<span className="text-xs text-ink-3">minimal sets of asserted facts</span>}>
      <form
        className="grid gap-2 sm:grid-cols-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (s.trim() && p.trim() && o.trim()) ex.mutate();
        }}
      >
        <Input aria-label="Subject" placeholder="issue:421" value={s} onChange={(e) => setS(e.target.value)} className="h-8 font-mono text-[12.5px]" spellCheck={false} />
        <Input aria-label="Relation" placeholder="handled_by_team" value={p} onChange={(e) => setP(e.target.value)} className="h-8 font-mono text-[12.5px]" spellCheck={false} />
        <Input aria-label="Object" placeholder="team:payments" value={o} onChange={(e) => setO(e.target.value)} className="h-8 font-mono text-[12.5px]" spellCheck={false} />
        <div className="flex items-center gap-2 sm:col-span-3">
          <Button size="sm" type="submit" icon={<Play className="size-4" aria-hidden />} busy={ex.isPending} disabled={!s.trim() || !p.trim() || !o.trim()}>
            Explain
          </Button>
          {ex.error ? <p className="min-w-0 truncate text-[13px] text-danger">{errorText(ex.error)}</p> : null}
        </div>
      </form>
      {j ? (
        <div className="mt-3">
          <div className="flex flex-wrap items-center gap-2">
            <FactChip fact={j.fact} derived={j.status === "derived"} />
            <Badge tone={j.status === "not_entailed" ? "danger" : j.status === "derived" ? "accent" : "neutral"}>{j.status === "not_entailed" ? "not derived" : j.status}</Badge>
          </div>
          <p className="tnum mt-1 text-xs text-ink-3">
            {plural(j.justifications.length, "justification")}
            {j.complete ? ", every one listed" : ", maybe more"}
            {j.stopped ? ` · stopped: ${j.stopped}` : ""}
            {j.fixpoint_reached ? "" : " · closure is a lower bound"}
          </p>
          <ol className="mt-2 flex flex-col gap-2">
            {j.justifications.map((x, i) => (
              <li key={i} className="rounded-input bg-sunken p-2.5">
                <p className="flex flex-wrap items-center gap-1.5 text-xs text-ink-3">
                  <span className="font-medium text-ink-2">#{i + 1}</span>
                  {[...x.rules, ...(x.builtin_rules ?? [])].map((r) => (
                    <span key={r} className="rounded bg-raised px-1 font-mono text-ink-2 shadow-card">
                      {r}
                    </span>
                  ))}
                  <span>{x.sufficient ? "sufficient" : j.verification ? "sufficiency not established" : "not sufficient"}</span>
                  <span>· {x.minimal ? "minimal" : j.verification || !x.sufficient ? "minimality not established" : "not minimal"}</span>
                </p>
                <ul className="mt-1.5 flex flex-col gap-1">
                  {x.facts.map((f, k) => (
                    <li key={k}>
                      <FactChip fact={f} />
                    </li>
                  ))}
                </ul>
              </li>
            ))}
          </ol>
          {j.verification ? <p className="mt-1 text-xs text-ink-3">{j.verification}</p> : null}
          <Means>{j.means}</Means>
        </div>
      ) : null}
    </Card>
  );
}

function parseManifest(text: string): CertManifest | undefined {
  try {
    return JSON.parse(text) as CertManifest;
  } catch {
    return undefined;
  }
}

function CertifyCard() {
  const cert = useMutation({ mutationFn: () => call<Certified>("reason.certify", {}) });
  const c = cert.data;
  const manifest = c ? parseManifest(c.certificate.manifest_json) : undefined;
  const check = c?.check;
  return (
    <Card
      title="Certificate"
      aside={
        <Button size="sm" variant="primary" icon={<BadgeCheck className="size-4" aria-hidden />} busy={cert.isPending} onClick={() => cert.mutate()}>
          {c ? "Certify again" : "Certify"}
        </Button>
      }
    >
      <p className="max-w-[80ch] text-[13px] text-ink-2">
        Writes every derivation as a line an independent checker (<span className="font-mono">onto-cert</span>) re-derives by pattern alone, then runs that checker. Only its verdict
        carries the words entailed or checked.
      </p>
      {cert.error ? <p className="mt-2 text-[13px] text-danger">{errorText(cert.error)}</p> : null}
      {c && check ? (
        <div className="mt-3 flex flex-col gap-3">
          {check.ok ? (
            <Verdict tone="ok" word={check.verdict} aside={<span className="font-mono text-xs text-ink-3">{check.checker}</span>}>
              <p>{check.means}</p>
              <p className="tnum mt-1 text-xs text-ink-3">
                {plural(check.asserted, "asserted fact")} · {plural(check.derivations, "derivation")} · rules cited: {check.rules_cited.builtin} built-in, {check.rules_cited.supplied} supplied
              </p>
            </Verdict>
          ) : (
            <Verdict tone="bad" word={check.error === "malformed" ? "malformed" : `rejected: ${check.reason}`}>
              {check.first_rejected ? (
                <p>
                  Line {check.first_rejected}
                  {check.rule ? (
                    <>
                      {" "}
                      citing <span className="font-mono">{check.rule}</span>
                    </>
                  ) : null}
                  {check.premise ? `, premise ${check.premise}` : ""}
                  {check.conclusion ? (
                    <>
                      : <span className="font-mono">{check.conclusion}</span>
                    </>
                  ) : null}
                  {check.premises?.length ? (
                    <span className="mt-1 block font-mono text-xs text-ink-3">
                      {check.premises.map((x) => (
                        <span key={x} className="block">
                          from {x}
                        </span>
                      ))}
                    </span>
                  ) : null}
                </p>
              ) : (
                <p>
                  {check.file ? `${check.file}${check.line ? `:${check.line}` : ""}` : ""} {check.field ? `field ${check.field}` : ""} {check.error === "malformed" ? check.reason : ""}
                </p>
              )}
            </Verdict>
          )}
          {manifest ? (
            <dl className="grid gap-x-4 gap-y-1.5 text-[12.5px] sm:grid-cols-[max-content_minmax(0,1fr)]">
              <Digest label="asserted.tsv" value={manifest.derived.asserted_sha256} />
              <Digest label="rules.tsv" value={manifest.derived.rules_sha256} />
              <Digest label="derivations.tsv" value={manifest.derived.derivations_sha256} />
              <dt className="text-ink-3">attested</dt>
              <dd className="tnum min-w-0 text-ink-2">
                {manifest.attested.engine} · schema v{manifest.attested.schema_version} · graph revision {manifest.attested.graph_revision} · {manifest.attested.principal}
                {manifest.attested.workspace ? ` · ${manifest.attested.workspace}` : ""} · {manifest.attested.created_at}
              </dd>
            </dl>
          ) : null}
          <div className="flex flex-wrap gap-2">
            <DownloadButton name="rules.tsv" text={c.certificate.rules_tsv} type="text/tab-separated-values" />
            <DownloadButton name="asserted.tsv" text={c.certificate.asserted_tsv} type="text/tab-separated-values" />
            <DownloadButton name="derivations.tsv" text={c.certificate.derivations_tsv} type="text/tab-separated-values" />
            <DownloadButton name="manifest.json" text={c.certificate.manifest_json} type="application/json" />
          </div>
          <p className="text-xs text-ink-3">
            Re-check offline with <span className="font-mono">onto-cert check &lt;dir&gt;</span>. A digest binds the certificate to these bytes, not to a state of the world; the attested fields are the engine's claims.
          </p>
        </div>
      ) : null}
    </Card>
  );
}

function Digest({ label, value }: { label: string; value: string }) {
  return (
    <>
      <dt className="font-mono text-ink-3">{label}</dt>
      <dd className="min-w-0 truncate font-mono text-ink-2" title={value}>
        sha256 {value}
      </dd>
    </>
  );
}
