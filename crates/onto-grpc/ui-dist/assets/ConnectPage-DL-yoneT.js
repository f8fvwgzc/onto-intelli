import{a as e,n as t,o as n,s as r,t as i,u as a}from"./consoleUi-BgXbk6nz.js";import{Ct as o,E as s,G as c,H as l,J as u,N as d,P as f,St as p,T as m,V as h,Z as g,gt as _,ht as v,i as y,lt as b,q as x,wt as S,xt as C,z as w}from"./index-ccEZlXcA.js";var T=S(o(),1),E=p(),D=[{value:`curl`,label:`curl`},{value:`typescript`,label:`TypeScript`},{value:`python`,label:`Python`},{value:`mcp`,label:`MCP`},{value:`env`,label:`Env vars`},{value:`monorepo`,label:`Monorepo`}];function O(e,t){let n=`Who owns Project Alpha?`;switch(e){case`curl`:return[{label:`Ask a question`,code:`curl -s "${t.api}/v1/ask" \\
  -H "authorization: Bearer ${t.key}" \\
  -H "content-type: application/json" \\
  -H "x-onto-session-id: chat-42" \\
  -d '{"question": "${n}"}'`},{label:`Run any action`,code:`curl -s "${t.api}/v1/call/entity.search" \\
  -H "authorization: Bearer ${t.key}" \\
  -H "content-type: application/json" \\
  -d '{"query": "payments", "limit": 5}'`}];case`typescript`:return[{label:`fetch, no dependencies`,code:`const res = await fetch("${t.api}/v1/ask", {
  method: "POST",
  headers: {
    authorization: \`Bearer \${process.env.ONTO_API_KEY}\`,
    "content-type": "application/json",
    "x-onto-session-id": "chat-42",
  },
  body: JSON.stringify({ question: "${n}" }),
});
const { answer, mode, trace_id } = await res.json();`},{label:`With the onto-intelli SDK (sdk/typescript)`,code:`import { Onto } from "onto-intelli";

// Reads ONTO_URL and ONTO_API_KEY when no options are given.
const onto = new Onto({ url: "${t.api}", apiKey: process.env.ONTO_API_KEY });
const res = await onto.ask("${n}", { sessionId: "chat-42" });
console.log(res.answer, res.trace_id);`}];case`python`:return[{label:`Standard library only`,code:`import json, os, urllib.request

req = urllib.request.Request(
    "${t.api}/v1/ask",
    data=json.dumps({"question": "${n}"}).encode(),
    headers={
        "authorization": f"Bearer {os.environ['ONTO_API_KEY']}",
        "content-type": "application/json",
        "x-onto-session-id": "chat-42",
    },
)
with urllib.request.urlopen(req) as res:
    body = json.load(res)
print(body["answer"], body["trace_id"])`},{label:`With the onto_intelli SDK (sdk/python)`,code:`import os
from onto_intelli import Onto

onto = Onto(url="${t.api}", api_key=os.environ["ONTO_API_KEY"])
res = onto.ask("${n}", session_id="chat-42")
print(res["answer"], res["trace_id"])`}];case`mcp`:return[{label:`Claude Code`,code:`claude mcp add --transport http onto-intelli ${t.mcp} \\
  --header "Authorization: Bearer ${t.key}"`},{label:`mcp.json (any MCP client with Streamable HTTP)`,code:JSON.stringify({mcpServers:{"onto-intelli":{type:`http`,url:t.mcp,headers:{Authorization:`Bearer ${t.key}`}}}},null,2)}];case`env`:return[{label:`.env`,code:`ONTO_URL=${t.api}
ONTO_API_KEY=${t.key}
# Optional: a key already implies its project.
ONTO_TENANT=${t.ref}`}];case`monorepo`:return[{label:`Map a monorepo's packages, owners and dependencies`,code:`# The onto CLI talks to this server with an admin-scope key.
export ONTO_REMOTE=${t.api}
export ONTO_TOKEN=${t.key}

# A git URL; crawled again every hour (manifests and CODEOWNERS only, never source files).
onto connect repo https://github.com/acme/platform --name platform --every 1h \\
  --owner @acme/payments=team:payments`}]}}var k={mcp:`MCP clients act with the key's scopes; an agent-scope key reads, proposes schema changes and, when the server allows writes, runs action types.`,monorepo:`Registering a connector needs an admin-scope key. Keys register https:// git URLs on public hosts only; a local path or an internal git server needs an operator token or the CLI on the server.`,typescript:`Never put a key in browser code: call from your backend, or mint a short-lived session with POST /v1/auth/session.`};function A(){let o=l(),p=C(),{project:S,role:A}=u(),j=S?.ref??``,M=D.find(e=>e.value===o.query.get(`tab`))?.value??`curl`,N=_({queryKey:x.connect(j),queryFn:()=>c.connect(j),enabled:!!S}),[P,F]=(0,T.useState)(),[I,L]=(0,T.useState)(),R=M===`mcp`?[`agent`]:M===`monorepo`?[`admin`]:[`read`,`write`],z=M===`mcp`?`MCP`:M===`monorepo`?`Monorepo`:`Service`,B=v({mutationFn:()=>c.createKey(j,{name:`${z} key ${new Date().toISOString().slice(0,10)}`,scopes:R}),onSuccess:e=>{L({prefix:e.key.prefix,secret:e.secret,scopes:e.key.scopes}),F(e.key.prefix),p.invalidateQueries({queryKey:x.connect(j)}),p.invalidateQueries({queryKey:x.keys(j)})}}),V=e=>M!==`monorepo`||e.includes(`admin`),H=(N.data?.keys??[]).filter(e=>V(e.scopes)),U=(P&&H.some(e=>e.prefix===P)?P:void 0)??(I&&V(I.scopes)?I.prefix:void 0)??H[0]?.prefix,W=I&&U===I.prefix?I.secret:`${U??`onto_sk_xxxxxxxx`}_…`;return(0,E.jsxs)(E.Fragment,{children:[(0,E.jsx)(y,{title:`Connect`,context:`Snippets for your services, agents and monorepo, with this project's URL and key`,children:(0,E.jsxs)(n,{size:`sm`,variant:`primary`,role:A,min:M===`monorepo`?`admin`:`developer`,busy:B.isPending,icon:(0,E.jsx)(b,{className:`size-4`,"aria-hidden":!0}),onClick:()=>B.mutate(),children:[`Create `,M===`mcp`?`an agent`:M===`monorepo`?`an admin`:`a`,` key`]})}),(0,E.jsx)(e,{children:N.error?(0,E.jsx)(m,{error:N.error,retry:()=>void N.refetch()}):N.data?(0,E.jsxs)(`div`,{className:`mx-auto flex max-w-4xl flex-col gap-4`,children:[B.error?(0,E.jsx)(`p`,{className:`text-[13px] text-danger`,children:g(B.error)}):null,I?(0,E.jsx)(r,{secret:I.secret,note:`The snippets below include it until you leave this page.`}):null,(0,E.jsxs)(`div`,{className:`grid gap-3 sm:grid-cols-[1fr_auto] sm:items-end`,children:[(0,E.jsx)(s,{label:`API key`,hint:I&&U===I.prefix?`The snippets hold this new key's full secret until you leave the page.`:H.length?`Snippets show the key's prefix; replace … with the secret you saved when the key was created.`:void 0,children:e=>(0,E.jsxs)(d,{id:e,value:U??``,onChange:e=>F(e.target.value),disabled:!H.length,children:[H.length?null:(0,E.jsx)(`option`,{value:``,children:M===`monorepo`?`No admin-scope keys yet: create one`:`No keys yet: create one`}),H.map(e=>(0,E.jsxs)(`option`,{value:e.prefix,children:[e.name,` · `,e.prefix,` · `,e.scopes.join(`, `)]},e.prefix))]})}),(0,E.jsx)(`a`,{href:w([`keys`]),className:`pb-2 text-[13px] text-accent hover:underline`,children:`Manage keys`})]}),(0,E.jsx)(`dl`,{className:`grid gap-x-6 gap-y-2 text-[13px] sm:grid-cols-3`,children:[[`API URL`,N.data.api_url],[`MCP URL`,N.data.mcp_url],[`Project ref`,N.data.ref]].map(([e,t])=>(0,E.jsxs)(`div`,{className:`min-w-0`,children:[(0,E.jsx)(`dt`,{className:`text-xs text-ink-3`,children:e}),(0,E.jsx)(`dd`,{className:`truncate font-mono text-[12.5px]`,title:t,children:t})]},e))}),(0,E.jsxs)(i,{children:[(0,E.jsx)(a,{label:`Snippet language`,value:M,onChange:e=>h({tab:e===`curl`?void 0:e}),tabs:D}),(0,E.jsxs)(`div`,{role:`tabpanel`,className:`mt-4 flex flex-col gap-3`,children:[O(M,{api:N.data.api_url,mcp:N.data.mcp_url,ref:N.data.ref,key:W}).map(e=>(0,E.jsx)(t,{label:e.label,code:e.code},e.label)),k[M]?(0,E.jsx)(`p`,{className:`text-xs text-ink-3`,children:k[M]}):null]})]}),(0,E.jsxs)(`p`,{className:`text-xs text-ink-3`,children:[`Every request becomes a trace on`,` `,(0,E.jsx)(`a`,{className:`text-accent hover:underline`,href:w([`traces`]),children:`Traces`}),`. Send `,(0,E.jsx)(`code`,{className:`font-mono`,children:`x-onto-session-id`}),` to group a conversation and a W3C `,(0,E.jsx)(`code`,{className:`font-mono`,children:`traceparent`}),` to join your own traces.`]})]}):(0,E.jsx)(f,{className:`h-64 rounded-card`})})]})}export{A as default};