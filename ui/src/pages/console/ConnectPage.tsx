import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { KeyRound } from "lucide-react";

import { Card, CodeBlock, Page, RoleButton, SecretReveal, Tabs } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { ErrorState, Field, Select, Skeleton } from "../../components/ui";
import { errorText } from "../../lib/api";
import { consoleApi, qk, useConsoleScope, type KeyScope } from "../../lib/console";
import { href, setQuery, useRoute } from "../../lib/router";

type Tab = "curl" | "typescript" | "python" | "mcp" | "env" | "monorepo";

const TABS: { value: Tab; label: string }[] = [
  { value: "curl", label: "curl" },
  { value: "typescript", label: "TypeScript" },
  { value: "python", label: "Python" },
  { value: "mcp", label: "MCP" },
  { value: "env", label: "Env vars" },
  { value: "monorepo", label: "Monorepo" },
];

interface Ctx {
  api: string;
  mcp: string;
  ref: string;
  key: string;
}

function snippets(tab: Tab, c: Ctx): { label: string; code: string }[] {
  const q = "Who owns Project Alpha?";
  switch (tab) {
    case "curl":
      return [
        {
          label: "Ask a question",
          code: `curl -s "${c.api}/v1/ask" \\
  -H "authorization: Bearer ${c.key}" \\
  -H "content-type: application/json" \\
  -H "x-onto-session-id: chat-42" \\
  -d '{"question": "${q}"}'`,
        },
        {
          label: "Run any action",
          code: `curl -s "${c.api}/v1/call/entity.search" \\
  -H "authorization: Bearer ${c.key}" \\
  -H "content-type: application/json" \\
  -d '{"query": "payments", "limit": 5}'`,
        },
      ];
    case "typescript":
      return [
        {
          label: "fetch, no dependencies",
          code: `const res = await fetch("${c.api}/v1/ask", {
  method: "POST",
  headers: {
    authorization: \`Bearer \${process.env.ONTO_API_KEY}\`,
    "content-type": "application/json",
    "x-onto-session-id": "chat-42",
  },
  body: JSON.stringify({ question: "${q}" }),
});
const { answer, mode, trace_id } = await res.json();`,
        },
        {
          label: "With the onto-intelli SDK (sdk/typescript)",
          code: `import { Onto } from "onto-intelli";

// Reads ONTO_URL and ONTO_API_KEY when no options are given.
const onto = new Onto({ url: "${c.api}", apiKey: process.env.ONTO_API_KEY });
const res = await onto.ask("${q}", { sessionId: "chat-42" });
console.log(res.answer, res.trace_id);`,
        },
      ];
    case "python":
      return [
        {
          label: "Standard library only",
          code: `import json, os, urllib.request

req = urllib.request.Request(
    "${c.api}/v1/ask",
    data=json.dumps({"question": "${q}"}).encode(),
    headers={
        "authorization": f"Bearer {os.environ['ONTO_API_KEY']}",
        "content-type": "application/json",
        "x-onto-session-id": "chat-42",
    },
)
with urllib.request.urlopen(req) as res:
    body = json.load(res)
print(body["answer"], body["trace_id"])`,
        },
        {
          label: "With the onto_intelli SDK (sdk/python)",
          code: `import os
from onto_intelli import Onto

onto = Onto(url="${c.api}", api_key=os.environ["ONTO_API_KEY"])
res = onto.ask("${q}", session_id="chat-42")
print(res["answer"], res["trace_id"])`,
        },
      ];
    case "mcp":
      return [
        {
          label: "Claude Code",
          code: `claude mcp add --transport http onto-intelli ${c.mcp} \\
  --header "Authorization: Bearer ${c.key}"`,
        },
        {
          label: "mcp.json (any MCP client with Streamable HTTP)",
          code: JSON.stringify({ mcpServers: { "onto-intelli": { type: "http", url: c.mcp, headers: { Authorization: `Bearer ${c.key}` } } } }, null, 2),
        },
      ];
    case "env":
      return [
        {
          label: ".env",
          code: `ONTO_URL=${c.api}
ONTO_API_KEY=${c.key}
# Optional: a key already implies its project.
ONTO_TENANT=${c.ref}`,
        },
      ];
    case "monorepo":
      return [
        {
          label: "Map a monorepo's packages, owners and dependencies",
          code: `# The onto CLI talks to this server with an admin-scope key.
export ONTO_REMOTE=${c.api}
export ONTO_TOKEN=${c.key}

# A git URL; crawled again every hour (manifests and CODEOWNERS only, never source files).
onto connect repo https://github.com/acme/platform --name platform --every 1h \\
  --owner @acme/payments=team:payments`,
        },
      ];
  }
}

const TAB_NOTES: Partial<Record<Tab, string>> = {
  mcp: "MCP clients act with the key's scopes; an agent-scope key reads, proposes schema changes and, when the server allows writes, runs action types.",
  monorepo: "Registering a connector needs an admin-scope key. Keys register https:// git URLs on public hosts only; a local path or an internal git server needs an operator token or the CLI on the server.",
  typescript: "Never put a key in browser code: call from your backend, or mint a short-lived session with POST /v1/auth/session.",
};

export default function ConnectPage() {
  const route = useRoute();
  const qc = useQueryClient();
  const { project, role } = useConsoleScope();
  const ref = project?.ref ?? "";
  const tab = (TABS.find((t) => t.value === route.query.get("tab"))?.value ?? "curl") as Tab;
  const info = useQuery({ queryKey: qk.connect(ref), queryFn: () => consoleApi.connect(ref), enabled: Boolean(project) });
  const [picked, setPicked] = useState<string>();
  const [fresh, setFresh] = useState<{ prefix: string; secret: string; scopes: KeyScope[] }>();
  // The monorepo snippet registers a connector, which needs an admin-scope key.
  const scopes: KeyScope[] = tab === "mcp" ? ["agent"] : tab === "monorepo" ? ["admin"] : ["read", "write"];
  const kind = tab === "mcp" ? "MCP" : tab === "monorepo" ? "Monorepo" : "Service";
  const create = useMutation({
    mutationFn: () => consoleApi.createKey(ref, { name: `${kind} key ${new Date().toISOString().slice(0, 10)}`, scopes }),
    onSuccess: (r) => {
      setFresh({ prefix: r.key.prefix, secret: r.secret, scopes: r.key.scopes });
      setPicked(r.key.prefix);
      void qc.invalidateQueries({ queryKey: qk.connect(ref) });
      void qc.invalidateQueries({ queryKey: qk.keys(ref) });
    },
  });

  // On the monorepo tab only admin-scope keys can run the snippet.
  const fits = (sc: KeyScope[]) => tab !== "monorepo" || sc.includes("admin");
  const keys = (info.data?.keys ?? []).filter((k) => fits(k.scopes));
  const prefix =
    (picked && keys.some((k) => k.prefix === picked) ? picked : undefined) ??
    (fresh && fits(fresh.scopes) ? fresh.prefix : undefined) ??
    keys[0]?.prefix;
  const key = fresh && prefix === fresh.prefix ? fresh.secret : `${prefix ?? "onto_sk_xxxxxxxx"}_…`;

  return (
    <>
      <PageHeader title="Connect" context="Snippets for your services, agents and monorepo, with this project's URL and key">
        <RoleButton
          size="sm"
          variant="primary"
          role={role}
          min={tab === "monorepo" ? "admin" : "developer"}
          busy={create.isPending}
          icon={<KeyRound className="size-4" aria-hidden />}
          onClick={() => create.mutate()}
        >
          Create {tab === "mcp" ? "an agent" : tab === "monorepo" ? "an admin" : "a"} key
        </RoleButton>
      </PageHeader>
      <Page>
        {info.error ? (
          <ErrorState error={info.error} retry={() => void info.refetch()} />
        ) : !info.data ? (
          <Skeleton className="h-64 rounded-card" />
        ) : (
          <div className="mx-auto flex max-w-4xl flex-col gap-4">
            {create.error ? <p className="text-[13px] text-danger">{errorText(create.error)}</p> : null}
            {fresh ? <SecretReveal secret={fresh.secret} note="The snippets below include it until you leave this page." /> : null}
            <div className="grid gap-3 sm:grid-cols-[1fr_auto] sm:items-end">
              <Field label="API key" hint={
                  fresh && prefix === fresh.prefix
                    ? "The snippets hold this new key's full secret until you leave the page."
                    : keys.length
                      ? "Snippets show the key's prefix; replace … with the secret you saved when the key was created."
                      : undefined
                }>
                {(id) => (
                  <Select id={id} value={prefix ?? ""} onChange={(e) => setPicked(e.target.value)} disabled={!keys.length}>
                    {keys.length ? null : (
                      <option value="">{tab === "monorepo" ? "No admin-scope keys yet: create one" : "No keys yet: create one"}</option>
                    )}
                    {keys.map((k) => (
                      <option key={k.prefix} value={k.prefix}>
                        {k.name} · {k.prefix} · {k.scopes.join(", ")}
                      </option>
                    ))}
                  </Select>
                )}
              </Field>
              <a href={href(["keys"])} className="pb-2 text-[13px] text-accent hover:underline">
                Manage keys
              </a>
            </div>
            <dl className="grid gap-x-6 gap-y-2 text-[13px] sm:grid-cols-3">
              {[
                ["API URL", info.data.api_url],
                ["MCP URL", info.data.mcp_url],
                ["Project ref", info.data.ref],
              ].map(([k, v]) => (
                <div key={k} className="min-w-0">
                  <dt className="text-xs text-ink-3">{k}</dt>
                  <dd className="truncate font-mono text-[12.5px]" title={v}>
                    {v}
                  </dd>
                </div>
              ))}
            </dl>
            <Card>
              <Tabs label="Snippet language" value={tab} onChange={(v) => setQuery({ tab: v === "curl" ? undefined : v })} tabs={TABS} />
              <div role="tabpanel" className="mt-4 flex flex-col gap-3">
                {snippets(tab, { api: info.data.api_url, mcp: info.data.mcp_url, ref: info.data.ref, key }).map((s) => (
                  <CodeBlock key={s.label} label={s.label} code={s.code} />
                ))}
                {TAB_NOTES[tab] ? <p className="text-xs text-ink-3">{TAB_NOTES[tab]}</p> : null}
              </div>
            </Card>
            <p className="text-xs text-ink-3">
              Every request becomes a trace on{" "}
              <a className="text-accent hover:underline" href={href(["traces"])}>
                Traces
              </a>
              . Send <code className="font-mono">x-onto-session-id</code> to group a conversation and a W3C <code className="font-mono">traceparent</code> to join your own traces.
            </p>
          </div>
        )}
      </Page>
    </>
  );
}
