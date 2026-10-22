# onto-intelli for TypeScript

A client for an [onto-intelli](../../README.md) server: ask questions of
your ontology, run governed actions, read and write entities, and read the
traces the server records. Zero dependencies, `fetch` only: Node 18+, Deno,
Bun. The source is TypeScript with erasable syntax only, so Node runs it
directly with type stripping.

## Install

From a checkout (the package is not on npm yet):

```sh
(cd sdk/typescript && npx -p typescript@5.8 tsc -p tsconfig.json)   # builds dist/, which the package exports
npm install /path/to/onto-intelli/sdk/typescript                     # in your project
```

Or copy `src/` into your code base: it has no dependencies.

## Connect

Create a project and an API key in the console (**API keys**), or ask your
operator for one. The **Connect** page shows this code with your project's
URL and key prefix filled in.

```ts
import { Onto } from "onto-intelli";

// With no options: ONTO_URL, ONTO_API_KEY and ONTO_TENANT.
const onto = new Onto({ url: "https://onto.example.com", apiKey: process.env.ONTO_API_KEY });

const res = await onto.ask("Who owns the payments service?", { sessionId: "chat-42", userId: "user:7" });
console.log(res.answer, res.mode, res.trace_id);
```

| Option | Environment | Meaning |
|---|---|---|
| `url` | `ONTO_URL` | Base URL of the server. Required. |
| `apiKey` | `ONTO_API_KEY` | An `onto_sk_…` API key or an `onto1.…` session. Sent as `authorization: Bearer …`. Not needed against `onto serve --dev`. |
| `tenant` | `ONTO_TENANT` | Project ref, sent as `x-onto-tenant`. Optional with an API key, which implies its project. |
| `workspace` | | Default workspace (`x-onto-workspace`). |
| `sessionId` | | Sent as `x-onto-session-id` on every request; the console groups traces by it. |
| `headers` | | Extra headers on every request. |
| `fetch` | | A `fetch` implementation (default: the global one). |
| `timeoutMs` | | Time to the response headers, per attempt. Default 30 000. |
| `maxRetries` | | Retries of 429 and 503 answers. Default 2. |
| `maxRetryDelayMs` | | The longest wait between retries, whatever `retry-after` says. Default 30 000. |

The key lives in a private field: it is never logged and does not appear in
`console.log(onto)` or `JSON.stringify(onto)`.

## Calls

```ts
await onto.ask(question, { sessionId, userId, metadata, tags, mode, conversationId, noCache, confirm, signal });
for await (const ev of onto.askStream({ question, conversationId: "new" })) {
  // ev.type: "stage" | "entities" | "delta" | "done" | "error"
  if (ev.type === "delta") process.stdout.write(ev.text);
}
await onto.call("graph.impact", { id: "service:payments" });   // any action: GET /v1/actions lists them
await onto.search("payments", { kinds: ["service"], limit: 10 });

await onto.entities.get(id);  .find(spec);  .upsert(input);  .patch(patch);  .delete(id);  .history(id);
await onto.links.create({ from, rel, to });  .delete({ from, rel, to });
await onto.actions.list();  .plan(action, params);  .apply(action, params, { confirm: true });  .undo(receipt);
await onto.objects.types();  .explore({ kind: "customer", search: "ada", limit: 20 });

await onto.traces.list({ since: Date.now() - 86_400_000, status: "error" });  .get(id);
await onto.traces.metrics({ bucket: "day" });  .score(traceId, { name: "user.feedback", value: 1 });
await onto.sessions.list();  .get(id);
await onto.auth.mintSession({ actAs: "user:7", roles: ["viewer"], ttlSecs: 900 });
```

Results have the server's field names (snake_case, the Rust serde shapes);
`src/types.ts` declares them (`AskResponse`, `AskEvent`, `Entity`,
`TraceRow`, `TraceDetail`, `TraceMetrics`, …). Option objects are camelCase
and converted (`userId` → `user_id`); `since` and `until` take a number of
Unix milliseconds or a `Date`.

Reading traces needs a credential that may observe: an `admin`-scope key or
a console session. Registering connectors needs an admin.

## Errors and retries

Every failure is an `OntoError` with `status` (HTTP status, or 0 when no
answer came: network failure, timeout, abort), `code` (the server's code:
`invalid_argument`, `unauthenticated`, `permission_denied`, `not_found`,
`conflict`, `resource_exhausted`, `unavailable`, `internal`; without a
response `unavailable`, `deadline_exceeded` or `cancelled`), `message`,
`traceId` (also `trace_id`: the `x-onto-trace-id` of a traced call) and
`retryAfter`.

```ts
import { OntoError } from "onto-intelli";

try {
  await onto.ask("…");
} catch (e) {
  if (e instanceof OntoError && e.code === "resource_exhausted") console.log(`budget reached, retry in ${e.retryAfter} s`);
  else throw e;
}
```

429 (a rate limit or budget) and 503 answers are retried up to twice,
waiting what `retry-after` says (capped by `maxRetryDelayMs`), else 0.5 s
then 1 s. `askStream` retries only before the stream starts.

## Browsers

Never put an API key in browser code. Call onto-intelli from your backend,
or mint a short-lived session there and hand it to the browser:

```ts
// backend
const { token, expires_at } = await onto.auth.mintSession({ actAs: `user:${user.id}`, roles: ["viewer"], ttlSecs: 900 });
// browser
const browser = new Onto({ url: "https://onto.example.com", apiKey: token });
```

A session minted with a key is bound to it and stops working when the key
is revoked.

## Tests

```sh
cd sdk/typescript
npm test                                  # offline, against a node:http mock server
ONTO_URL=http://127.0.0.1:50051 npm test  # also the live test, e.g. against `onto serve --dev`
```

`npm test` runs `node --experimental-strip-types --test` (Node 22.6+ for the
tests; the package itself needs Node 18+). `npm run build` writes `dist/`
with `tsc` (TypeScript 5.8+) for publishing; the tests do not need it.
