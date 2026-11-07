# onto-intelli phase 2: functions (user code in a WASM sandbox)

Status: implementation design, revision 2. Revision 1 synthesized three independent drafts
(security-minimal, author-first, scale-ops) and two judges' reviews of them; section 13 records which
draft each part came from, every conflict and how it was resolved, and the drafts' claims that
verification corrected. Revision 2 applies a three-lens review (security, governance,
compatibility and performance); "Review decisions" at the end records each finding and what changed.
Written against the worktree `onto-intelli-wt/verify-345`, branch `feat/phase2-functions`. PR #8
(phase 1) is merged, and this branch's base is an ancestor of `origin/main`, which is 30 commits
ahead (Knowledge, Signals, the #8 review fixes, and `a10d5b6`, which hides events about entities the
reader cannot read). Step 0 (section 11) rebases onto main and re-checks every line reference; until
then, line numbers come from the worktree. Every file, type and function named as existing was read
in that tree. Facts about wasmi were read in the wasmi 2.0.0 sources in the local cargo registry
(`~/.cargo/registry/src/index.crates.io-*/wasmi-2.0.0`). Numbers marked *measured* come from two
scratch crates built on this Mac on 2026-10-29 with `wasmi =2.0.0` and `default-features = false`:
the security-minimal draft's (features `std, validate, deterministic`: 7 crates not in today's
Cargo.lock, a 6.1 s clean debug build, a 130 MB `target/`, a clean `cargo deny --offline check` with
the repo's `deny.toml`), and the compatibility review's (D1's features: the tail-call stack overflow
in debug, A.1; with `portable-dispatch` added, about 120 ns per fuel unit in debug and 1.3 ns in
release; per-instance costs, A.5). Nothing else is built, and no cargo command was run in the
worktree. Each remaining number is an estimate, and its build step says how it is measured.

## 0. Scope

The roadmap item: "User code in a WASM sandbox with time and memory limits. It reads the ontology and
returns edits that still go through governed actions. It powers function-backed actions, computed
properties and query functions."

Phase 2 ships in this order. Each slice is usable on its own:

1. **Sandbox**: a new leaf crate `onto-sandbox` (the wasmi interpreter, no host imports, fuel and
   memory limits), and the Rust guest crate `onto-fn` with one example function.
2. **Definitions and artefacts**: a `[functions.<name>]` schema section, governed like every other
   section, and content-addressed module bytes in a new append-only tenant table (migration 0015).
3. **Query functions and computed properties**: both read-only and evaluated as the caller.
   Computed values are opt-in on every surface.
4. **Function-backed actions**: a function returns edits, and the existing plan, confirmation,
   receipt and undo path applies them. A confirmation binds to the edits the person saw.
5. **Automations** that reach function-backed action types: a closure item, plus retries for
   changed inputs and spent budgets.
6. **Author toolchain**: typed bindings, `onto fn types | build | test | diff | publish`, fixture
   tests that run in the real sandbox, previews of old against new code in schema plans, and a test
   console in the UI.
7. **Surfaces**: the actions registry, HTTP, gRPC, MCP, CLI, SDKs and UI.

Deferred out of phase 2 (section 9 keeps the design notes): TypeScript through Javy (phase 2.1,
owner decision 1), host read imports (`oi-fn/2`), Python, the component model, writeback from
functions, functions as preconditions or automation conditions, sort and filter on computed values,
materialised computed values, artefact garbage collection, metering function fuel to the control
plane, definer rights, payloads on function events, and out-of-process isolation per tenant.

## 1. Decisions in one place

**How the base was chosen.** The judges split. One picked the security-minimal draft, for the
smallest trusted computing base, claims that held against the tree, and the only measured
dependency cost. The other picked the author-first draft, because a feature that accepts only Rust,
approved as an opaque digest with no binding between confirmation and commit, will see little use.
Both agreed on the core the three drafts share. This design starts from **security-minimal**, for
three reasons:

1. **Its weaknesses are additive.** The author layer (a fixture test runner, typed bindings,
   previews, signatures, confirmation bound to edits) grafts onto a zero-import ABI unchanged.
2. **Author-first's weaknesses are structural.** It batches subjects into one instance and shares
   per-item cache entries across readers, it lets action inputs `find` under the write lock, and it
   depends on an unverified vendored Javy plugin.
3. **TypeScript loses little.** Javy needs WASI imports. Section 9 designs that import set as its
   own reviewed rule set, `oi-wasi/1`. Its timing is owner decision 1, with phase 2.1 recommended.
   Nothing in phase 2 has to change for it: the input and output documents stay the same, and only
   the transport differs.

| # | Decision | Why (short) |
|---|---|---|
| D1 | **Runtime: wasmi 2.0**, pinned `=2.0.0`, `default-features = false`, features `std`, `validate`, `deterministic`, `extra-checks`, **`portable-dispatch`**. The workspace builds the wasmi crates at `opt-level = 3` in the dev profile. It lives in a new leaf crate `crates/onto-sandbox`, with `#![forbid(unsafe_code)]` and no `onto-*` dependency. onto-core always depends on it: there is no cargo feature. A hub whose tenants do not trust each other keeps functions off until process isolation exists (A.8, section 9). | *Measured:* 7 new crates, 6 s, 130 MB debug target, cargo-deny clean. Its MSRV is 1.86, at or below the repo's 1.94 (Cargo.toml:18). Without `portable-dispatch`, wasmi uses tail-call dispatch in every profile, and a debug build overflows the native stack after a few thousand guest instructions (A.1). An interpreter has no JIT, so it has no JIT miscompile class, but it has had a memory-safety bug (CVE-2025-66627, A.8). CI tests default features only (ci.yml:21, 48, 117), so a feature would ship an untested "not built in" path. |
| D2 | **Determinism is strict.** Fuel is the only limit on a function; there is **no wall-clock failure class**. Instantiation is charged in fuel too (A.4). Modules compile with `CompilationMode::Eager`. Floats use canonical NaNs. Inputs and outputs are canonical JSON, written by `canonical_json` (automate.rs:631), never by `serde_json`'s map order. Time enters only when declared, bucketed. Created ids come from the receipt seed. | wasmi charges translation fuel on a function's first call under lazy translation (code_map/mod.rs:860-871), so only Eager gives the same fuel cold and warm. The same call stops at the same instruction on every replica and OS. That is what lets the commit reuse a planned output and lets the cache keep failures. |
| D3 | **ABI `oi-fn/1` has zero imports.** A module exports `memory`, `oi_alloc(i32) -> i32` and `oi_run(i32, i32) -> i64`, and optionally `oi_panic() -> i64` (G.5). One JSON document goes in and one comes out. A module with any import is refused at upload. **Each call gets a fresh store and instance; calls are never batched.** | With no host functions there is nothing to call back into: no re-entrancy, no lock held across a call, no clock, randomness, network or secret. A fresh instance per subject means no state can carry one reader's data into another reader's value. |
| D4 | **Reads are declared, not called.** Each function declares an input manifest: roots (entity parameters, `self`, or a bounded `find` for queries only), relation walks (kinds, depth ≤ 3, limit, order) and selected properties. The host builds the input natively under one read guard, **as the caller**, through `policy::visible` (with `also` on dry runs). A walk cut at its limit says `more`. `on_limit = "error"` refuses instead. **Action functions read only items that are neither restricted nor private** unless the definition declares `restricted = "include"` (B.2). The work of building an input is capped and charged: `limits.scan` counts every edge and candidate examined, hidden ones included (B.2). | One consistent snapshot. Visibility is enforced in the one place the engine already trusts. Reviewers can read what the code reads. Action inputs never `find`, so the rebuild under the write lock never scans a kind. "Cut and say so" keeps working as data grows, and it does not lie. The caps on items bound the output; the scan cap bounds the work, under the write lock too. |
| D5 | **Guest code never runs under a graph guard or inside `transact`.** The graph, text and vector locks become a private `CountedRwLock` newtype whose only `read()` and `write()` return counting guards, so every acquisition is counted. The count is checked before every guest run: in debug builds a non-zero count panics, and in release builds the call is refused. | `transact` holds the write lock while `build` runs, up to 33 times (engine_sync.rs:20, 42-65), and about a dozen other sites take the lock directly (A.6). With the lock behind the newtype there is no uncounted path, so the rule holds by construction rather than by a list of wrapped call sites. |
| D6 | **The definition and the input digest pin the commit.** `FunctionRun` records the definition's hash and the input digest. Inside `transact` a changed definition, or a rebuilt input with a different digest, is `Conflict("inputs_changed: …")` and nothing is written; a switch that is now off is a final `functions_off` check. When `revision()` has not moved since `prepare`, the rebuild is skipped. The guest never re-runs at commit. `apply_action` re-prepares outside the lock, at most 3 attempts in all. | Same code and same input give the same output (D2), so an equal definition hash and digest mean the planned edits are exactly what the function would return now. The commit's cost is at most a scan-capped native walk plus one SHA-256, and usually nothing. |
| D7 | **Output is literal edits in a closed vocabulary**: `patch`, `link`, `unlink`, `create`, `delete`, `event`. There is no `replace` and no event payload. They become ordinary action-type effects, planned by the same per-effect code as templated effects (`plan_effects`, extracted from action_types.rs:1065-1310) with `Templates` in a literal mode that renders nothing. There is no writeback and no raw `Mutation`. | One place for preview lines, inverses, irreversibility and risk. A `{x}` in user data stays data. `EntityInput.replace`, `provenance` and `workspace` stay out of reach. `link {replace}` would expand over links the function never read (B.4). |
| D8 | **Edit scope is least privilege.** An edit may name only the `id` of an entity object in its input (an entity parameter, `self`, a walk or find item), never an id found in `actor`, `parent`, a property or a text parameter. A created object is connected only through its own inline `links`. Kinds, relations and properties come only from the function's `writes` envelope, which may not name `acl`, `visibility` or `member_of`. Event names are `fn.<function>.<declared suffix>`. A created id that already exists refuses the plan. | No blind writes to guessed or hidden ids, so the commit-time digest covers every object an edit touches. Lint and the loop checks read the envelope. A function cannot forge `action.*`, `automation.*` or `authorization.*` events, and access control stays with people. |
| D9 | **The code sees and writes aliases only.** A function's `names` table maps aliases to schema kinds, relations and qualified `kind.property` names. The existing rename passes rewrite the right-hand sides, including subtype and interface targets. Entity ids are opaque strings: a kind rename changes their prefix (old ids redirect), so guests never parse them. | Renames never reach the compiled code's aliases. The same table is the read and write allow-list, and it is what codegen emits. |
| D10 | **Invoker rights only.** A function reads and writes as whoever calls it. There is no `function:` principal, no change to `RESERVED_ID_PREFIXES` (policy.rs:227) and no change to the Tutela contract. | No new trust machinery. A function cannot see or do more than its caller. |
| D11 | **Confirmation binds to edits.** A function-backed plan carries `edits_digest`, the SHA-256 of the canonical returned edits, taken before ids are assigned. For a plan that needs confirmation, `confirm` must come with `expect = edits_digest`, or the plan is not applied. `ApplyOptions` gains `expect` and loses `Copy`. | `confirm` is a bool today, and apply plans afresh (action_types.rs:1434-1490). `drift()` compares no mutations (844-859), so with user code, "what you saw" and "what commits" could differ. |
| D12 | **Sandbox results are cached by content.** The key is `sha256("oi-fn/1" ‖ engine id ‖ artifact ‖ canonical(limits) ‖ input bytes)`, held in a per-tenant LRU under a process-wide byte total with fair-share eviction. The value is what the sandbox produced: the raw output bytes and fuel, or a sandbox fault. Decoding and every rule that depends on the definition or the graph run on each use, after the cache. Concurrent misses for one key run once (single flight). Transient failures and internal faults are never cached. There is no invalidation. A module compiled for one tenant is used by another only after that tenant's own artefact row is verified. | The input already holds everything the output may depend on, including what this reader can see. So a key that includes it can neither leak across readers nor go stale. Writes pay nothing. A link-only or remote commit, which no listener sees by entity (engine.rs:1549-1561, engine_sync.rs:120-133), cannot leave a stale value either. A refusal that depends on the envelope or on graph state is never kept past the change that fixes it. |
| D13 | **Module bytes live in an append-only tenant table** (`0015_function_artifacts.sql`), with refuse-update, refuse-delete and no-truncate triggers. Uploads insert if absent by `sha256`. The schema stores only the digest. Bytes are re-hashed on every load. No precompiled code is ever stored, and nothing is garbage-collected in phase 2. An operator purge (C.1) is the only delete. | Schema versions and proposals copy the whole document (live_schema.rs:745-772, proposals.rs:71-80). Revert and undo need old code. Re-hashing catches tampering. `refuse_delete` already lets a purge or a future retention sweep through (0008:7-13), so neither needs a migration. |
| D14 | **Governance is the schema.** `put_function` and `delete_function` go through `schema.plan`, `propose` and `apply`. Agents may propose, never apply or approve (live_schema.rs:102-113). Swapping code is at least medium risk. Plans and proposals preview old against new code on up to 20 visible subjects, computed for each reader and never stored. Automation closures gain a `function:<name>` item. There is no enablement state for functions. | The digest is in the definition, so approving the change approves the exact bytes. The preview shows reviewers behaviour, not just a digest, and only over what each reviewer may see. A code swap pauses the automations that reach it until a person enables them again. |
| D15 | **Writer fence.** This build sets `WRITER_VERSION = 2`. Every schema commit whose resulting schema holds a function, a function-backed action type or a computed property raises `head.min_writer` to 2 in the same transaction. Every replica must run this build before the first `put_function`. | In the common case a phase-1 replica cannot even read such a tenant: it cannot deserialize `put_function` or compile a function-backed action type, so its catch-up and reads fail loudly (C.9). The fence covers the remaining case, a later ordinary head version that the old build could reload without the `functions` section (no `deny_unknown_fields` on `Schema`, schema.rs:513) and commit. The column and the check exist already (0009_changes.sql:45, postgres.rs:672-683). |
| D16 | **A function-backed action type** sets `function = "<name>"` instead of `effects` (exactly one of the two). Roles, parameters and preconditions stay declarative. The declared risk is a floor the output can only raise. `unattended` must be empty. Undo uses the inverses derived natively. | Plans, confirmation, receipts and undo keep their meaning. Code cannot talk risk down. |
| D17 | **Computed properties** attach as `[object_types.<t>.computed]` and are evaluated on read, as the reader. They are opt-in on every surface (`computed: true` or a list of names), including MCP `get_entity`. Results go in their own `computed` map; failures go in a sibling `computed_errors` map, never as `null`. Each request has a fuel budget and a cap on evaluations. They are never stored, never in the change feed, never on as-of or history reads, and never sortable or filterable in phase 2. | `derived` stays exactly as it is, and MCP's forced `derived = true` (mcp.rs:948-950) never triggers user code. A clean, typed `computed` shape for clients. `null` means "unset" (schema.rs:131-134), so a failure must never look like one. |
| D18 | **Query functions** are read-only, may `find`, and are called by name. They carry a **signature hash**. `expect_signature` refuses a changed one. The output type is checked. They are in `GRAPH_READS`, so they also run on what-if forks. | Apps in product repos pin a stable, reviewed contract and get a refusal rather than a different shape. Scenarios compare the live graph with a fork. |
| D19 | **Errors have classes** (B.7). Deterministic failures are final: a failed plan check `function`, or `Invalid` with a `function_*` prefix. Plan-check text is written by the host and never carries guest text. `inputs_changed` is a retryable `Conflict`. No capacity is `Unavailable`. A spent tenant or principal budget is `Limited("functions: … retry in N s")`, and `runs::retryable` gains a rule that retries it at the stated refill time. `functions_off` is final for calls; an automation that meets it is paused once and its run is recorded as `skipped`, so it can be rerun (D.6). | `retryable` (runs.rs:477-483) never retries `Limited` today. HTTP must never send `Retry-After: 60` for an overrun that would fail again (error.rs:54-63). A kill must not silently drop a materialisation's updates. |
| D20 | **Limits** are declared per function and must stay within the process caps; they are checked at plan, propose and apply, never at load. Each tenant has a fuel bucket per process. Each principal may draw at most a share of it per minute (less for agents). Two reserves keep classes apart: background callers (automation runs) stop above `background_reserve_pct`, and reads (queries, computed values, tests, previews) stop above `write_reserve_pct`, so only function-backed actions may drain it. Concurrency is capped per process, per tenant and per principal. Per-project overrides come from the control plane's project settings JSON, with no migration; `0` there means zero, not "unset". | The same split as `max_automations` (engine.rs:47-51): a stored schema always opens, and calls refuse loudly. A backfill cannot starve people's reads, and no reader or agent can starve the tenant's writes or other people. `limits_of` already reads project settings (onto-control llm.rs:135-149), but treats 0 as unset (142), which function limits must not. |
| D21 | **Execution is inline** on the calling thread, behind a process semaphore handed out round-robin by tenant, a per-tenant cap and a per-principal cap. Fuel runs in slices (wasmi resumable calls); the switches, the digest denylist and shutdown are checked at each slice boundary. Sliced fuel equals unsliced fuel, which needs onto-sandbox's own resource limiter (A.4). A run is wrapped in `catch_unwind`, and every slot, charge and single-flight entry is an RAII guard. | No pool and no handoff. Dispatch already runs engine work inside `blocking()` (onto-intelli actions.rs:1982-1987). Slicing gives preemption points without epochs and without a nondeterministic timeout. `extra-checks` turns interpreter bugs into panics, which must not wedge waiters or leak slots. |
| D22 | **Off by default.** There are five switches: process `[functions] enabled`; an operator setting `functions.enabled` in the project settings, which tenant admins cannot override; a tenant kill (admins only, never agents); per-function off; and a process-wide digest denylist, reloaded from a file without a restart. A digest whose runs panic the interpreter is quarantined on that process. | Matches `[writeback] enabled` (engine.rs:52-56) and the phase-1 switches. An operator who suspects one tenant or one module can stop it on every replica without stopping everyone, and a tenant admin cannot undo that. |
| D23 | **Authoring in phase 2 is Rust**, through the in-repo guest crate `onto-fn`, with generated typed bindings (`function.types`), fixture-graph tests that run in the real sandbox, a determinism double run, fuel and memory headroom, and `--record`. TypeScript is phase 2.1 (owner decision 1). | It keeps D3, and it still gives authors a typed, testable loop. Javy modules import WASI preview 1, which `oi-fn/1` refuses on purpose. |
| D24 | **Surfaces are generic**: one `function.*` action family, one `FunctionService`, and the MCP tools `list_functions`, `call_function` and `test_function`. There is no tool or RPC per function. | The MCP table is static (`tools()`, mcp.rs:71; `listChanged: false`, mcp.rs:905) and gRPC routes are built once (routes.rs:236). |
| D25 | **Guest text stays with the caller.** A function's `error`, `message`, `log` and panic text go to the caller's response, the receipt (its actor and admins) and traces with `capture_content`. They never reach plan-check text, audit events, run rows or `automation.run` events, which wider audiences read. Plan-check text names the class, the function, rule names, edit indexes, the input digest and fuel, never a value the guest produced. `message` is stripped of control and format characters, NFC-normalised and shown apart from the host's preview lines. | `reject()` copies failed checks into the `action.<name>` event (action_types.rs:1570-1584), and a run's reason goes into `automation.run` (runs.rs:749-767), both readable across the workspace. A privileged caller's function could otherwise move restricted values there by accident or on purpose. |

Philosophy carried over from phase 1:

- Every graph write goes through `Ontology::apply`/`transact` with an `ActionContext` and becomes a
  receipt. Functions only return edits.
- Agents propose and never enable, approve or apply schema.
- Refuse rather than lie: a failed computed value is listed in `computed_errors`, a cut walk says
  `more`, a stale input or a changed definition refuses the commit, a changed edit set refuses a
  confirmation, and a `computed` argument on an as-of read is refused rather than ignored.
- Outputs are sorted and deterministic.

Words used in outputs: "returns edits" (never "writes"); "fuel" (never "time limit"); "inputs
changed" (never "conflict" alone); "computed" (never "derived") for function values. The existing
UI and MCP text that calls `derived` aggregates "computed" (EntityCard.tsx:101, mcp.rs:98,
schema.rs:218) is renamed "derived" in the step that adds `computed` to that surface.

## 2. What exists today (ground truth)

**Commit path and locks**

- `transact` (engine_sync.rs:42-65) takes the graph write lock (47) and runs `build` on the locked
  graph. On `Stale` it catches up and builds again, up to `MAX_RETRIES = 32` (20, 54-57). Once those
  run out, it returns `Conflict("write kept losing …")` without a prefix (59-61).
- `Extra` (engine.rs:213-235) carries KV, events, schema, fence, outbox rows, run rows and timers into
  the same commit. `Mutation` is Upsert, Patch, Delete, Link, Unlink or Event (model.rs:274-286).
  `stage()` checks write policy (engine.rs:1635) and validates against the schema (1641).
- Listeners run only when a commit changed entities, links or schema, or enqueued outbox rows
  (engine.rs:1549-1561). `ChangeSet.entities` is `staged.touched`. A catch-up fires one `ChangeSet`
  with no entities (engine_sync.rs:120-133). An invalidation scheme driven by listeners would
  therefore miss link-only and remote commits.
- `Ontology::read` holds the read guard for its closure, without filtering or any `Access::Read`
  check (engine.rs:635-640). `inner_graph_read` (701-703) is a second guard path (used by
  authority.rs:361, 423, 446 and learn.rs:494, 586). `reader(p)` checks `Access::Read` and may
  `sync_if_behind` (705-713). `get` returns `not_found` for an entity the caller may not see
  (717-724). `neighbors` sorts its output (757). `fork()` deep-clones into a capture-off
  `MemoryStore` (644-675). `Graph` hashes with `RandomState` (graph.rs:5, 18-21), so iteration order
  is not stable across processes. `estimated_bytes` is cached by revision (engine.rs:1130-1153).
- Besides those paths, the graph lock is taken directly at engine.rs:638, 645, 678, 702, 712, 933,
  1002, 1070, 1136, 1153, 1158 and 1227 (fork, `resolve_id`, quality, `version_visible`, a scope
  set, `estimated_bytes`, counts, stats, merge) and at engine_sync.rs:48 (`transact`) and 72
  (`sync()`, a second write path). main adds one more read in `event_target_visible`. The lock is
  a task-fair parking_lot `RwLock` (engine.rs:8): a waiting writer blocks new readers.

**Visibility and principals**

- `policy::visible` (policy.rs:332-355) checks the workspace and lets privileged principals see
  everything. A `restricted` or `private` entity needs the principal, or a `member_of` team, in its
  `acl`, so visibility depends on the principal's own links too: each restricted candidate costs a
  scan of the principal's `member_of` links (351). `ACL_LINK_TYPES = ["member_of"]` (315), and
  `check_acl_link` lets only privileged principals write them (321). `scope_key` includes the
  principal id (214-218). `RESERVED_ID_PREFIXES` is at 227.
- `Graph` keeps one adjacency list per entity for all relations together (`out`, `inc`,
  graph.rs:24-25), so `steps_via` filters a node's whole degree on every call (213-242), and
  `of_kind` iterates a whole kind (244).
- `Ontology::events` filters by workspace only in this worktree (engine.rs:1114-1124). On main,
  `a10d5b6` also hides events whose target the reader cannot read; payloads are never filtered.

**Action types**

- `ParamType` and `ParamDef` are at action_types.rs:51-82.
- `Effect` holds exactly one of patch, link, unlink, create, delete, event or writeback (189-213,
  enforced at 300-313). `ActionType` (216-241) has no function field. `validate` requires at least
  one effect (283-286) and runs in `compile()` (schema.rs:795). `primary_param` takes the first
  effect's subject (action_types.rs:270-281).
- `CreateEffect` (147-160) makes `{kind}:{key}`, with the key defaulting to the seed's `new_id`, and
  carries inline `links`. A create whose id already exists becomes an irreversible update with a
  warning (1232-1235).
- `Templates` (543) is the one renderer: `render` (609), lenient `render_value` (615), `render_props`
  (619) and `render_strict` (626). Every entity lookup goes through `see`.
- `PlanSeed {now, new_id}` with `PlanSeed::of(receipt_id, ts)` (507-525) reproduces a plan's values
  on re-plan. `ApplyOptions {dry_run, confirm}` derives `Copy` (496-505). Outside tests it is built
  as a full literal at onto-intelli actions.rs:976 (`action.apply`), 1411 (`learned.certify` and
  `learned.reject`, confirm true) and command.rs:146 (`ask {confirm}`, from intelli.rs:88 and 1214),
  and with `..Default::default()` at onto-intelli seed.rs:387; tests build it at action_types.rs:1770,
  modeling.rs:2172, learn.rs:907 and in tests/{writeback,execution,packs,v02,action_plans}.rs. The
  MCP `apply_action` schema has `confirm` and no other option (mcp.rs:321-325).
- `link` effects with `replace` unlink every other link of that relation on the raw graph, with no
  visibility check, and name each in the preview as `(replacing X)` (action_types.rs:1134-1151).
  Unlink staging checks write access on `from` only (engine.rs:1812-1820); link staging also checks
  read on `to` (1903-1904).
- `reject()` marks a receipt rejected and writes an `action.<name>` failure event whose payload holds
  the params and the failed check texts (1570-1584). A receipt is readable by its actor and by admins
  of its workspace (1588-1594).
- A plan with no mutations and no external writes is invalid, with the warning "nothing to change"
  (1312-1315).
- `plan_on_as` (939ff) works in this order:
  - checks roles and the write right (956-966);
  - normalises parameters with `see = visible(p) && also.is_none_or(visible(a))` (951);
  - evaluates preconditions (1061-1064);
  - plans each effect in one loop (1065-1310): patch at 1074, an irreversible state change at 1103,
    create at 1204, delete raising risk to High at 1277, writeback at 1301.
- Then `needs_confirmation = risk >= Medium || !reversible` (1316), and external writes force
  confirmation unless `unattended` (1318-1330).
- `apply_action` (1434-1491) plans under `self.read` (1444), returns unapplied without `confirm`
  (1476-1480), refuses an agent's confirmation (1482-1488), and calls `apply_planned` (1503-1565).
  That re-plans inside `transact` with the receipt's seed and refuses with `precondition_changed`
  when `drift()` (844-859) finds the plan invalid, riskier or no longer reversible. `drift()`
  compares no mutations. `undo_action` (1615) applies the stored inverses.

**Automations**

- `closure()` (onto-core automate.rs:686-725) hashes the automation, each action type it reaches
  (`hash_of(at)`, 656) and each endpoint and target, over `canonical_json` (631).
- `evaluate_on` (runs.rs:544) plans with `plan_on_as` as the run principal (574) and turns a plan
  that needs confirmation into `NeedsConfirmation` (583-591). `evaluate_run` (682-685) is used
  before claiming (onto-intelli automate.rs:459) and for dry runs (972). `commit_run` evaluates again
  inside `transact` (runs.rs:700-760).
- runs.rs already defines a type named `Prepared` (391-398), so this design names its type
  `FunctionRun`.
- `retryable()` (477-483) retries `Unavailable` (except a lost lease) and any `Conflict` not
  prefixed `run_settled` or `precondition_changed`, and **nothing else**. `execute_run` (897-927)
  retries with `retry.delay_ms(attempts)` through a timer.
- At commit, `NotMet` and `Invalid` give a `failed` row with reason `precondition_changed: …`
  (runs.rs:749-751), and the row's `summary()`, reason included, is the payload of the
  `automation.run` event (318-332, 759-767). The run row itself is readable only by principals that
  dominate `run_as` (onto-intelli automate.rs:800-806), so the event is the wider channel.
- Before the claim, the consumer turns `Invalid` into a `refused` row, `plan_invalid: …`, and
  `NotMet` into no row at all (onto-intelli automate.rs:459-472). `pause_automation` (234) pauses
  an automation once with an event (`automation.paused`, `automation.stale`). A `skipped` row can be
  run again with `automation.rerun` (runs.rs:92-93).
- `run_as` may hold any role but system and agent, so admin is allowed (automate.rs:496-507). An
  automation never triggers itself unless `allow_self` is set (353-357).
- The consumer runs a round per `spawn_blocking` (onto-intelli automate.rs:147). A run's trace is
  built afterwards from its row (284-304).

**Schema**

- `Schema` (schema.rs:513-535) has `automations` with `skip_serializing_if` (528-531). Neither
  `Schema` nor `SchemaFile` (539) uses `deny_unknown_fields`. `SchemaChange` is a closed
  `#[serde(tag = "op")]` enum with no catch-all variant (live_schema.rs:142-143), and `read_version`
  deserializes a version and then compiles it, mapping either failure to a storage error
  (3261-3267). Catch-up propagates that error (engine_sync.rs:107-110), and `reader` calls
  `sync_if_behind()?` (engine.rs:705-712).
- A kind rename retypes entity ids from `{from}:…` to `{to}:…`, with old ids redirecting
  (live_schema.rs:2284-2296). `RenameProperty` rewrites every kind that `kind_is` the renamed type
  (1094).
- A proposal stores its whole `SchemaPlan` (proposals.rs:84). `without_reader_facts` replaces only
  `consequences` and `shapes` before storage (live_schema.rs:586-596, called at proposals.rs:289 and
  528), and `with_reader_view` restores the per-reader fields (600-607). `list_proposals` returns
  stored plans to any reader without re-planning (proposals.rs:308-341).
- `schema_commit` runs plan, `check_migration` and `check_automations` inside `transact`
  (live_schema.rs:3644-3647); slow work is hoisted before the lock (`early`, 3638-3641).
  `check_automations` judges the change, not the whole schema (`check_limit(base, new, …)`, 3561).
- `ObjectType.derived` (218-220) holds `DerivedDef`, which is exactly one aggregate (metrics.rs:34-58,
  75-77).
- `compile()` (schema.rs:672) checks automations for structure only (798-801).
- `changes_between` (live_schema.rs:1554-1600) sends any section it does not name to the glossary
  branch (1583-1596).
- `Governance::can_apply`, `can_approve` and `can_propose` are at 102-113. In config mode, API
  changes are refused (124, 3594).
- `schema_plan_as` (3497) builds shape facts under the read guard, then drops it before the slow
  part (3510-3527). `check_automations` is at 3559 and `assess_risk` at 3146.
- The undo "still present" match is at 3852-3868.
- `locks::SECTIONS` is at locks.rs:48-58, with the touched mapping at 284-294. The pack layer names
  are at packs.rs:404.
- `ProposalView.preview` re-plans against the active schema as the viewer (proposals.rs:118-129).

**Derived values**

- `Ontology::derived` (metrics.rs:618-625) computes under `self.read` with `visible` inputs, adding
  `age_days` and `idle_days` from `now_ms()` (121-131).
- MCP `get_entity` declares `derived` default true (mcp.rs:99) and forces it (948-950). Its
  description says it returns "computed properties" (98), and the doc comment on
  `ObjectType.derived` calls them "Computed properties" (schema.rs:218).
- `EntityCard` asks for `derived: true` (ui/src/components/EntityCard.tsx:38) and shows them under a
  section titled "Computed" (101).
- `entity.history` and `entity.as_of` (onto-intelli actions.rs:191-192, 987-998) and
  `map.snapshot` (map.rs:136) read past versions; their inputs are parsed leniently.
- The reasoner lists some properties as `NOT_EVALUATED` (reason.rs:81).

**Errors**

- `Invalid`, `Conflict`, `Unavailable` and `Limited` are at error.rs:5-32. `retry_after_secs` returns
  the N of "retry in N s", else 60 (54-63).

**Storage**

- Migrations 0001-0014 (`MIGRATIONS`, store/postgres.rs:70-85).
- `refuse_update()` (0003). `refuse_delete()` lets a `DELETE` through only when `onto.retention` is
  `on`, and is installed with TRUNCATE triggers (0008:1-13).
- `WRITER_VERSION = 1` (postgres.rs:89-92). `lock_head` refuses writes when
  `head.min_writer > WRITER_VERSION` (672-683). Nothing raises the column, which 0009 added
  (0009_changes.sql:45).
- A build refuses a tenant at a newer migration (postgres.rs:798-808).
- `WriteBatch` is at store/mod.rs:130. `CONFIG_NAMESPACES` (outbox.rs:39-40) limits what
  `commit_config` writes, and `refuse_configure` is at 398.

**Registry and dispatch (onto-intelli)**

- `AGENT_WRITES` (actions.rs:61), `agent_may_write` (87), `ACTIONS` (106), `admit` (928) and
  `dispatch` (933-953) routing phase-1 modules by `CALLS`.
- `blocking()` (1982-1987), `GRAPH_READS` (1991-2011) and `graph_call` (2014), whose `entity.get`
  adds `derived` only when asked (2016-2023).
- `traces_call` (849-855), `secret_bearing` (2130), and `system.health` (1449, from
  `outbound_health`, intelli.rs:508).

**Tenancy, limits and shutdown**

- `TenantSettings {llm, limits, meter, version, traces, secrets}` (hub.rs:50-63). `Limits` covers
  asks and LLM budgets only (limits.rs:24-30); limits are per process (1-12). `limits_of` reads
  project settings JSON (onto-control llm.rs:135-149) and treats `0` as unset (142).
- The Helm chart requests 256 MiB and limits the pod to 2 GiB (deploy/helm/onto-intelli/values.yaml:80-82).
  `tenant_memory_budget_mb` defaults to 0, so byte-based tenant eviction is off; `max_open_tenants`
  is 512 and `tenant_idle_secs` 1800 (onto-intelli config.rs:108-110).
- `Intelli::estimated_bytes` (intelli.rs:755-757) feeds `enforce_limits` (hub.rs:473-517).
- `is_stopping` (intelli.rs:727), `begin_shutdown` (hub.rs:377), `finish_shutdown` (394),
  `shutdown_grace_secs = 20` (onto-intelli config.rs:455), and `Released` (lifecycle.rs:41-51).
- `AutomationConfig::check` is at config.rs:1020-1040.

**Supply chain and CI**

- `deny.toml` checks `all-features = true`, `ignore = []`, `yanked = "deny"`, `unsound = "all"` and
  the licence list in the brief.
- `serde_json` 1.0.154 has no `indexmap` dependency today (Cargo.lock:2587-2597), so
  `serde_json::Map` is a `BTreeMap`. That holds only until some crate enables serde_json's
  `preserve_order` through feature unification; `indexmap` 2.14.2 is already in the lock (1244).
  `canonical_json` (automate.rs:631-648) sorts keys explicitly. There is no wasm runtime in
  Cargo.lock. `unicode-normalization` and `unicode-properties` are already in the lock.
- `deny.toml` advisories and `cargo audit` read the RustSec database only (ci.yml:119-130).
- CI runs fmt and clippy (ci.yml:20-21), test-postgres with the demo eval (25-55), the OS matrix
  (109-117), deny and audit (119-130), helm (132), docker (168) and the ui-dist diff (187-200). Tests
  run in the debug profile (48, 117), and the workspace has no `[profile.dev]` (Cargo.toml:85-94).
  No profile sets `panic`, so panics unwind.

**wasmi 2.0.0** (registry sources)

- `Config` has `consume_fuel`, `compilation_mode`, `allow_start_fn`, `ignore_custom_sections`,
  `floats`, `wasm_multi_memory`, `wasm_custom_page_sizes`, `wasm_wide_arithmetic`, `wasm_tail_call`,
  `wasm_extended_const`, `set_max_recursion_depth`, `set_max_stack_height` and `enforced_limits`
  (engine/config.rs:106-462).
- `wasm_simd`, `wasm_relaxed_simd` and `wasm_memory64` exist only with the `simd` and `memory64`
  features (config.rs:296-335). With those features off, the proposals are not compiled at all.
- `CompilationMode::{Eager, LazyTranslation (default), Lazy}` (config.rs:36-52); `Lazy` is documented
  as not deterministic.
- Lazy translation charges validation and translation fuel at first use (code_map/mod.rs:860-871).
- `StoreLimitsBuilder` offers `memory_size`, `table_elements`, `instances` and
  `trap_on_grow_failure` (limiter.rs:17-92).
- `call_resumable` (func/typed_func.rs:120) returns `TypedResumableCall::OutOfFuel` (resumable.rs:465),
  which can `resume` (584) and reports `required_fuel` (425).
- **Dispatch.** `dispatch/mod.rs:1-14` selects the tail-call backend whenever `portable-dispatch` is
  off and either `auto-dispatch` is off or the build sets `wasmi_use_tail_calls`. build.rs sets that
  only at opt-level s, z, 2 or 3, because "LLVM does not perform the required sibling-call
  optimization at opt-level 0 or 1". The tail backend ends each handler with `return handler(…)`
  (tail.rs:70-79; `become` only under `unstable`).
- **Grow and fuel.** `memory.grow` charges `bytes / 64` fuel (wasmi_core fuel.rs:25, 158-163). On a
  shortfall it calls the limiter's `memory_grow_failed` (wasmi_core memory/mod.rs:188-196, 236-243).
  `StoreLimits` returns `ResourceLimiterDeniedAllocation` there when `trap_on_grow_failure` is set
  (limiter.rs:160-165), which the executor turns into a `GrowthOperationLimited` trap, not a
  resumable out-of-fuel (exec.rs:427-431). Tables behave the same (694-697).
- **Instantiation.** A linear memory is allocated by writing zeros over its whole initial size
  (wasmi_core memory/buffer.rs:76-82), and data segments are copied, with no fuel charged.
- **Panics.** With `extra-checks`, `unreachable_unchecked!` panics instead of being undefined
  (engine/utils.rs:4-13).
- **Advisories.** GHSA-g4v2-cjqp-rfmq (CVE-2025-66627, published 2025-12-08, high) is a
  use-after-free in wasmi's linear memory on growth, affecting 0.41.0-1.0.0 (fixed in 0.41.2,
  0.47.1, 0.51.3 and 1.0.1). It has no RustSec entry, so `cargo deny` and `cargo audit` never
  reported it. The audits listed in the wasmi README cover 0.31.0 and 0.36.0-0.38.0, not the 2.0
  executor.

**Rollback precedent.** An older build refuses a tenant whose head version holds operations it
does not know, and phase 1 documents the removal steps (docs/STORAGE.md:122-136).

## 3. Ownership and boundaries

- **onto-intelli owns** functions on its own objects:
  - the sandbox;
  - artefacts and definitions;
  - input assembly;
  - query functions, computed properties and function-backed action types;
  - their quotas, switches, stats and traces;
  - the author toolchain (`onto fn`, the guest crate, codegen, the test console).
- **attache** owns every LLM call. A function cannot call a model, because it has no imports. An
  "AI function" is an attache agent that calls `call_function` and proposes or applies actions
  through the governed path.
- **texc-symphony** owns long-running work and schedules. A function is bounded by fuel (the cap is
  set from step 1's calibration to at most about 5 s on the worst instruction mix, A.5), and anything
  longer is refused, never queued. A schedule that needs a function calls
  `function.call` or `action.apply` from a texc-symphony job.
- **Tutela** owns identity and finer permissions. Invoker rights (D10) need no new principals, so
  nothing changes in the Tutela contract (CONTROL_PLANE.md §5). Definer rights wait for
  Tutela-issued service principals (owner decision 4).
- **Products** (Workshop- or Quiver-like apps in other repos) call query functions and
  function-backed actions by name over HTTP, gRPC or the SDKs, with generated client types. They
  never upload or host function code in their own process; app-specific functions live in the
  tenant schema.

---

## Part A. Runtime and sandbox (`crates/onto-sandbox`)

### A.1 Crate and dependencies

```toml
# crates/onto-sandbox/Cargo.toml
[dependencies]
wasmi = { version = "=2.0.0", default-features = false, features = ["std", "validate", "deterministic", "extra-checks", "portable-dispatch"] }
[dev-dependencies]
wat = "=1.x.y"   # exact version pinned in step 1; WAT test fixtures only
```

```toml
# Cargo.toml (workspace root): the interpreter is optimised in debug and test builds too
[profile.dev.package.wasmi]
opt-level = 3
[profile.dev.package.wasmi_core]
opt-level = 3
[profile.dev.package.wasmi_ir]
opt-level = 3
[profile.dev.package.wasmi_collections]
opt-level = 3
[profile.dev.package.wasmparser]
opt-level = 3
```

- The *measured* normal dependency graph:
  - wasmi, wasmi_core, wasmi_ir, wasmi_collections;
  - wasmparser 0.228;
  - spin 0.9.9 and libm 0.2.16;
  - bitflags, already in the lock.
- Licences are MIT/Apache-2.0, MIT, and "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT", all
  on the allow-list.
- Leaving out default features drops `wat` and its tree, `memory64` and `stable`. It does **not**
  give portable dispatch: with `auto-dispatch` off, wasmi selects its tail-call backend in every
  profile (section 2). That backend relies on LLVM's sibling-call optimisation, which LLVM does not
  perform at opt-level 0, so a debug build grows the native stack by a frame per executed
  instruction. *Measured* with exactly revision 1's features, a debug build and a 2 MiB thread (the
  test-thread default): a 5-instruction loop ran 1,000 iterations and aborted the process with a
  stack overflow at 3,000 (about 18k fuel). Release builds would be safe only while LLVM happens to
  optimise every handler. A host shared by tenants cannot depend on that, so **`portable-dispatch`
  is set explicitly**: the loop backend is safe in every profile. Tail-call dispatch is not used
  until Rust's `become` is stable.
- **The dev profile override.** *Measured* with `portable-dispatch`: 10M iterations (60M fuel)
  took 7.2 s in debug (about 120 ns per fuel unit) and 79 ms in release (about 1.3 ns). Without the
  override, a test near the 1B cap would take about 2 minutes per call on the CI runners. Building
  the five interpreter crates at opt-level 3 in the dev profile costs compile time, not new crates;
  step 1 records both the build time and the `target/` growth.
- `wat` is a dev-dependency only (onto-sandbox and onto-core tests). Step 1 records its crates and
  `target/` growth. If it adds more than 100 MB or a licence outside the list, the fixtures become
  committed `.wasm` files generated by `scripts/fn-fixtures.sh`, with their `.wat` sources next to
  them.
- The crate exposes no wasmi type in its public API: `Sandbox::new`, `compile`, `run`, `Rules`,
  `Limits`, `Outcome`, `Fault`, `ModuleInfo`, `StopFlags`. Inside, it implements its own
  `wasmi::ResourceLimiter` (A.4). That is about 600 lines. A future backend or an out-of-process
  worker (section 9) changes this crate only.
- **Bumping wasmi.** `=2.0.0` never moves by itself. docs/FUNCTIONS.md gains a bump procedure: watch
  the GitHub advisories for wasmi (osv-scanner in CI, section 6), bump to the fixed patch release,
  re-run the cross-OS vector and the calibration, and note in the release notes that the engine id
  (A.2), and therefore every cache key, changes.

### A.2 Engine configuration

There is one `wasmi::Engine` per process, configured once and shared by every tenant:

| Setting | Value | Why |
|---|---|---|
| `consume_fuel` | true | fuel is the only limit (D2) |
| `compilation_mode` | `Eager` | translation never happens during a call, so the fuel used is the same on a cold and a warm cache (code_map/mod.rs:860-871). `LazyTranslation` is the default and is not used; `Lazy` is refused |
| `allow_start_fn` | false | nothing runs at instantiation |
| `floats` | true | JSON numbers; canonical NaNs come from `deterministic` |
| `wasm_multi_memory`, `wasm_custom_page_sizes`, `wasm_wide_arithmetic` | false | a smaller interpreter surface. SIMD, relaxed SIMD and memory64 are not compiled in (their features are off, config.rs:296-335). Rust's default wasm32 features (bulk memory, sign extension, saturating float-to-int, multi-value, reference types, mutable globals) stay on |
| `ignore_custom_sections` | true | name and producer sections are skipped, never parsed |
| `enforced_limits` | functions, globals, data segments and table sizes bounded | validating a hostile module cannot itself be expensive |
| `set_max_recursion_depth`, `set_max_stack_height` | from `[functions] max_stack_kib` | engine-wide; a stack overflow traps |

The **engine id** is `wasmi-2.0.0/<sha256 of this table and the feature list>[..12]`. It goes into
every cache key (D12)
and every receipt's function block. It does **not** go into the closure item (C.4): an engine bump
is a fuel-cost change announced in the release notes, not a code change for people to re-approve.
Step 1's tests confirm each API name. If one differs, the setting stays and the call is renamed.

### A.3 Module rules (checked at upload and again at every load)

1. Size ≤ `caps.module_kib` (default 2 MiB).
2. It validates eagerly under the configuration above.
3. **The import section is empty.**
4. It has no start function, exactly one memory (32-bit, not shared), and at most one table with at
   most 10,000 elements.
5. **Initial memory is small.** The memory's initial size is at most `caps.initial_memory_kib`
   (default 2 MiB), and its data segments fit inside it. Growth at run time is charged in fuel; the
   initial size is not, so it is capped here and charged before the run (A.4). The `onto-fn`
   template sets a 64 KiB shadow stack (`-C link-arg=-zstack-size=65536`), so a typical guest starts
   at 2-3 pages instead of Rust's default 17.
6. Exports: `memory`; `oi_alloc` of type `(i32) -> i32`; `oi_run` of type `(i32, i32) -> i64`;
   optionally `oi_panic` of type `() -> i64` (G.5). Other exports (for example `__data_end`) are
   listed in the module info and never called.

A refusal names the rule and the offending item, for example `oi-fn/1 allows no imports; found
wasi_snapshot_preview1.fd_write (TypeScript arrives with oi-wasi/1, see docs/FUNCTIONS.md)`. The
stored module info holds:

- size, exports, function count, `initial_memory_bytes` and `data_bytes`;
- `rules = "oi-fn/1"`;
- `validated_by = <engine id>`.

`check_functions` (C.5) also refuses a definition whose `limits.memory_mb` is below the module's
initial memory.

### A.4 One call

```text
run(compiled, input_bytes, limits, stop: &StopFlags) -> Result<Outcome{output, fuel, memory_bytes, slices}, Fault>
  catch_unwind(AssertUnwindSafe(|| {                         // a panic is Fault::Internal (below)
  0. debug_assert / refuse: onto-core's guard counter is 0 (A.6)
  1. inst = (info.initial_memory_bytes + info.data_bytes) / 64   // instantiation, charged like bulk copies
     inst > limits.fuel -> Fault::OutOfFuel; remaining = limits.fuel - inst
  2. Store::new(engine) with onto-sandbox's Limiter {memory_bytes, tables: 1, table_elements: 10_000,
     instances: 1, memories: 1}; instantiate with an empty linker (no imports exist to link)
  3. p = oi_alloc(len) (sliced as in 4); check p..p+len lies inside memory; copy the input in
  4. set_fuel(min(slice_fuel, remaining)); call = oi_run.call_resumable(p, len)
     loop:
       Finished(r)        -> break
       OutOfFuel(c)       -> remaining -= fuel given in this slice
                             if remaining == 0 or c.required_fuel() > remaining -> Fault::OutOfFuel
                             if stop.off() (process, operator, kill, function off, denylist) -> Fault::Off
                             if stop.cut() -> Fault::Stopping
                             set_fuel(min(max(slice_fuel, c.required_fuel()), remaining)); call = c.resume(..)
       Err(trap)          -> if the limiter recorded a denial -> Fault::Memory
                             if Unreachable and oi_panic is exported -> read the panic text (G.5)
                             else Fault::Trap(code) or Fault::Stack
  5. out_ptr = (r >> 32) as u32, out_len = r as u32; out_len <= limits.output_bytes and
     out_ptr..out_ptr+out_len inside memory, else Fault::Output
  6. copy the output out; fuel = limits.fuel - remaining; memory_bytes = memory size now
  7. drop the store: nothing outlives the call
  }))
```

- Every call gets a fresh store and instance, and none is reused, so no state passes between calls,
  subjects or readers (D3).
- Faults: `OutOfFuel`, `Memory` (a grow over the limit), `Stack`, `Trap(code)`, `Panicked(text)`,
  `Output(why)`, `Off`, `Stopping`, `Internal`.
- **The limiter.** `trap_on_grow_failure(true)` is not used: wasmi routes a grow's fuel shortfall
  through `memory_grow_failed`, and `StoreLimits` would turn it into a trap (section 2), so a grow
  whose cost straddles a slice boundary would trap although the call's fuel covers it. onto-sandbox's
  own `Limiter` refuses in `memory_growing` and `table_growing` only for limit denials (recording
  the denial), and returns `Ok(())` from `memory_grow_failed` and `table_grow_failed` for
  `OutOfFuel`, so wasmi takes its resumable out-of-fuel path. A refused grow is a trap classed as
  `Fault::Memory` from the recorded denial, never confused with out of fuel.
- **Sliced fuel equals unsliced fuel.** A slice is resumed with at least the instruction's
  `required_fuel`, so a bulk operation or a grow larger than a slice still completes. `slice_fuel`
  is a process setting that is in neither the engine id nor the cache key, so it must not change an
  outcome. Step 1 asserts identical output, fuel and outcome digest for `slice_fuel` of 1,000, 10M
  and unlimited, over a loop, a recursion, a 16 MiB `memory.fill` (262,144 fuel, so only the
  1,000 slice makes `required_fuel` exceed a slice, which the test asserts happens), and a memory
  grow and a table grow whose cost straddles a slice boundary.
- **Instantiation is charged.** wasmi zeroes the whole initial memory and copies data segments
  without fuel (section 2). The pre-charge uses the same 64 bytes per fuel unit as bulk copies, so
  it counts against the call, the tenant bucket and `request_fuel`, and it is identical on every
  replica. With the 2 MiB initial cap (A.3), data segments included, it is at most 65,536 fuel.
- **Panics.** `extra-checks` turns an interpreter invariant violation into a panic, and the run is
  inline in dispatch. `catch_unwind` maps a panic to `Fault::Internal`: never cached, logged with the
  digest, and counted toward that digest's quarantine (I.2). The store is dropped during the unwind.
  The workspace keeps `panic = unwind` (no profile sets `panic`).

### A.5 Limits and fuel

Fuel counts instructions, and wasmi charges bulk-memory operations and grows by size (64 bytes per
fuel unit): a step-1 test checks that a 16 MiB `memory.fill` costs more than a 1 KiB one.
*Measured* with D1's final features in release on this Mac: about 1.3 ns per fuel unit on a simple
loop (60M fuel in 79 ms). So 50M fuel is roughly 65 ms and 1B fuel roughly 1.3 s on that loop. The
earlier 1 ns probe omitted `extra-checks` and `portable-dispatch` and is superseded.

A simple loop is the best case, so the caps are not set from it. **Step 1 calibrates**, in release
and in the debug test profile (with the A.1 override), the ns per fuel unit of the simple loop and
of an adversarial mix: `call_indirect` chains, `memory.grow` and shrink loops, tiny bulk operations,
deep recursion and instantiation of a module at the initial-memory cap. The test asserts that the
worst mix stays within a set factor (8×) of the simple loop; above it, the factor is raised only by
a reviewed change. The fuel caps, the defaults and the shutdown arithmetic (A.6) are derived from the
**worst** mix: `caps.fuel × worst ns per fuel` must stay at or below 5 s. Step 16 records the
CI-hardware numbers in docs/FUNCTIONS.md.

| Limit | Default (per call) | Process cap | Declared where |
|---|---|---|---|
| Module bytes | — | 2 MiB | cap only |
| Initial linear memory | from the module | 2 MiB | `caps.initial_memory_kib`, a module rule (A.3) |
| Linear memory | 16 MiB | 64 MiB | `limits.memory_mb` |
| Input scan (edges and candidates examined, hidden ones included) | 20,000 | 100,000 | `limits.scan` (B.2) |
| Fuel, action and query | 50M | 1B | `limits.fuel` |
| Fuel, computed | 10M | 100M | `limits.fuel` |
| Fuel slice (stop checks) | 10M | — | `[functions] slice_fuel` |
| Input bytes (canonical JSON) | 256 KiB | 1 MiB | `limits.input_kib` |
| Output bytes | 64 KiB | 256 KiB | `limits.output_kib` |
| Edits per output | 100 | 1,000 | `writes.max` |
| Entities in an input | sum of limits | 10,000 | the sum of every walk and find `limit`, checked at compile |
| Walk depth | 1 | 3 | `depth` |
| Log | 4 KiB | 4 KiB | fixed |
| Message and error text from the function | 256 characters | — | fixed |
| Computed functions per object type | 8 | — | fixed |
| Functions per tenant | 100 | `[functions] max_functions` | checked at apply and propose |

The worst-case guest memory in one process is `max_concurrent × caps.memory_mb` (default 4 × 64 MiB).
wasmi allocates linear memory as ordinary heap, with no virtual reservation. With the module cache
and the output-cache total, functions add up to `module_cache_mb + output_cache_total_mb +
max_concurrent × caps.memory_mb` of resident memory per process: 64 + 128 + 256 = 448 MiB with the
J defaults (revision 1's defaults came to about 0.9 GiB, almost half the Helm chart's 2 GiB limit).
`FunctionsConfig::check` logs that sum at startup and warns when it exceeds 25% of the cgroup
limit (`/sys/fs/cgroup/memory.max`, when present). Elapsed time is measured and reported, in traces
and stats, but never used to stop a call.

### A.6 Placement: never under a lock

- **The rule.** Guest code never runs while this thread holds a graph guard, read or write, and
  never inside a `transact` closure. Inputs are built under one short read guard, the guard is
  released, and then the guest runs.
- **The guard counter.** onto-core gets `thread_local! { static GUARDS: Cell<u32> }`.
  - `Inner.graph` becomes `CountedRwLock<Graph>`, a private newtype in `engine.rs` whose only
    `read()` and `write()` return `Held` guards that raise the counter on creation and lower it on
    drop. `Held` implements `Deref` (and `DerefMut` for writes), so the ~15 call sites, including
    `sync()`'s write path (engine_sync.rs:72) and main's `event_target_visible`, change by type only.
  - The `text` and `vectors` locks get the same newtype.
  - `Ontology::read` (engine.rs:635-640), `inner_graph_read` (701), `reader` (705) and `transact`
    (engine_sync.rs:47) are covered because they take the lock through it. No list of wrapped sites
    is kept: the raw lock is unreachable outside the newtype's module, so a new site cannot skip the
    counter.
  - `functions::runtime` checks `guards_held() == 0` before every guest run. In debug builds a
    non-zero count panics; tests run in debug, so CI enforces the rule. In release builds the call
    is refused (`Error::Other("function run under a graph guard")`) and logged.
- **Concurrency.**
  - A process semaphore `max_concurrent` (default 4), a per-tenant cap `max_concurrent_per_tenant`
    (default 2) and a per-principal cap `max_concurrent_per_principal` (default 1; a project whose
    app calls through one key raises it in its project settings).
  - The semaphore is handed out **round-robin by tenant**: a freed slot goes to the waiting tenant
    that least recently got one, so two busy tenants cannot keep a third waiting past `queue_ms`.
  - A call waits at most `queue_ms` (default 200), then gets
    `Unavailable("function_capacity: no sandbox slot within 200 ms")`.
  - The semaphore is `parking_lot::Mutex` plus `Condvar` in onto-core, because onto-core has no
    tokio dependency of its own.
  - **Every holding is an RAII guard**: the process and tenant slots, the principal slot, the bucket
    pre-charge (refunded on drop) and the single-flight entry (E.3). Their `Drop`, which also runs
    during an unwind, releases, refunds, removes the entry and calls `notify_all`, so a panic
    (A.4) never leaks a slot or wedges a waiter.
- **The thread.** A call runs on the calling thread. Every dispatch path already runs synchronous
  engine work inside `blocking()` (onto-intelli actions.rs:1982-1987), so a multi-thread runtime
  hands that worker's other tasks to another thread. Automation rounds run on `spawn_blocking`
  (onto-intelli automate.rs:147).
- **Shutdown.**
  - After `begin_shutdown` (hub.rs:377), new calls refuse with `Unavailable("stopping: …")`.
  - In-flight calls finish within their fuel: at most `caps.fuel × worst ns per fuel`, which the
    caps keep at or below 5 s (A.5), well inside `shutdown_grace_secs = 20`.
  - At the `finish_shutdown` deadline (hub.rs:394) the cut flag is set. A guest still running stops
    at its next slice with `Fault::Stopping` and has written nothing, because guests never write.
  - `Released` (lifecycle.rs:41-51) gains `functions`: the guests cut.

### A.7 Module cache

- The cache is process-wide, `digest → Arc<wasmi::Module>`, LRU by `module_cache_mb` (default 64).
  wasmi exposes no compiled size, so the estimate is 3 × the wasm bytes, calibrated in step 7.
- **A tenant uses it only for digests it holds.** Each `FnTenant` keeps `verified: HashSet<digest>`,
  filled only after that tenant's own `function_artifacts` row has been read and its SHA-256
  checked (`function_artifact_corrupt` if it differs). Every path (calls, tests, previews, uploads'
  validation) checks `verified` first; a digest not in it reads the row, and a missing row is
  `function_artifact_missing` on every replica, whether or not another tenant compiled the same
  bytes. Without this, a schema in config mode or a `function.test` draft could name another
  tenant's digest and run its code on chosen inputs, and the answer would depend on the replica.
- Behind that check, compiled modules are shared across tenants: the bytes are identical, and a
  module holds no tenant data. On a miss the verified bytes are validated against today's rules and
  compiled eagerly.
- Concurrent misses for one digest compile once (single flight). Nothing compiled is ever written
  back to PostgreSQL.
- Every load also checks the digest denylist and the quarantine (I.2).
- An accepted side channel: a tenant that uploads byte-identical code can learn, by timing its first
  call, that another tenant on the same process already compiled it. This is documented, not
  mitigated.

### A.8 Trusted computing base and threats

What must be correct for the sandbox to hold:

1. wasmi 2.0.0 and wasmparser 0.228. These are third party, with about 300 `unsafe` sites that
   onto-sandbox's `forbid(unsafe_code)` does not cover. An interpreter removes the JIT class but
   still has translation and executor memory-safety bugs: wasmi 0.41.0-1.0.0 had a use-after-free
   in linear memory growth with possible code execution (CVE-2025-66627, section 2), which RustSec
   never listed. The published audits cover 0.31.0 and 0.36.0-0.38.0, not the 2.0 handler-per-op
   executor.
2. `onto-sandbox`: about 600 lines, `forbid(unsafe_code)`.
3. The input builder (`onto-core/src/functions/input.rs`): visibility and manifest walks.
4. The output decoder and edit planner (`functions/output.rs` and the shared `plan_effects`).

Everything after the decoder is the existing engine: write policy in `stage()` (engine.rs:1635),
schema validation (1641), receipts and undo.

**Exposure.** Every writer and agent in every tenant can make the interpreter run a module of its
choice: `function.test` on a draft runs unapproved code, and a proposal's preview runs a proposer's
code as each viewer who asks for it. All tenants share one process. The design therefore treats a
module as hostile input from any writer: drafts are a tenant setting that is off for agents by
default (C.7), previews of unapproved code run only on request (C.5), CI checks GitHub advisories as
well as RustSec, a weekly differential fuzz job runs the pinned interpreter (section 6), and
operators have a digest denylist and a per-tenant switch that tenant admins cannot override (I.2).
**A hub whose tenants do not trust each other keeps functions off** until the out-of-process
option (section 9) exists; docs/FUNCTIONS.md says so.

| Threat | Control |
|---|---|
| Sandbox escape through an interpreter bug | No JIT. Pinned version, bumped by procedure (A.1). `extra-checks`. Our crate has no `unsafe`. cargo-deny and cargo-audit (RustSec) plus osv-scanner (GitHub advisories) in CI. A weekly differential fuzz job. Drafts off for agents. The operator denylist and switch; process isolation is a deferred option with a stated trigger (section 9). |
| Interpreter panic (`extra-checks`) | `catch_unwind` gives `Fault::Internal`; RAII guards release slots, refund charges and wake waiters; the digest is quarantined after repeated panics (A.4, A.6, I.2). |
| Parser or compile bomb | Size cap, `enforced_limits`, eager validation once per digest per process, custom sections skipped. Uploads hash first, skip validation for digests already stored, and are rate-limited per principal (C.2). |
| CPU exhaustion | Fuel per call, including instantiation (A.4). The scan cap on input building, charged in fuel (B.2). The tenant fuel bucket with per-principal shares and two reserves, process, tenant and principal concurrency caps, request budgets for computed values (I.1). |
| Memory exhaustion | The limiter, the initial-memory cap, the concurrency bound, LRU caches with byte caps that count in `estimated_bytes`, a process-wide value-cache total with fair-share eviction, and the startup memory sum (A.5). |
| Reading data the caller cannot see | The input is built as the caller through `visible` (with `also` on dry runs). Hidden entities are absent, never reported as "restricted"; `more` and order consider visible items only. The scan-cap refusal has one fixed text, so it does not reveal how much was hidden. |
| Writing outside the declared scope | Edits may name only the ids of entity objects in the input (D8); there is no `replace`. The `writes` envelope is enforced and may not name access-control properties or relations. `stage()` still checks write policy. |
| Exfiltration | No imports. Output reaches the caller (query, computed, test), edits the caller could make (actions), the receipt (its actor and admins) and traces with `capture_content`. Guest text never reaches plan-check text, events, run rows or `automation.run` (D25), and function events carry no payload (B.4). Action functions read no restricted or private item unless they declare `restricted = "include"`, and a plan that read one needs a person (B.2). The residual channel, the choice of edits itself, is open only to that confirmed case and to code the governance process approved. |
| Audit forgery | Event names are forced to `fn.<function>.<suffix>`, and the payload is written by the host. |
| Confused deputy: approved but malicious code run by an admin | Governance approves the exact digest, with the envelope, behaviour previews (C.5), confirmation for medium risk and above bound to the edits (D11), confirmation whenever a restricted item was read, no writes to access control, and the wording in section 7. |
| Cross-reader leak through state or the cache | A fresh instance per call (D3). The cache key includes the full input bytes (D12). Output caches are per tenant. |
| Cross-tenant use of the module cache | A tenant uses a compiled module only for digests whose rows it holds and has verified (A.7). |
| A guest run under a lock stalling the tenant | The counted lock (A.6), checked before every guest run; the structural test in section 6. |
| Input building stalling writers | The scan cap, the commit-time reuse when nothing moved, and the 2× bound on the rebuild (D.3). |
| Tampered bytes in the database | Re-hashed on load; a mismatch refuses with `function_artifact_corrupt`. |
| Error strings carrying data | Host-written plan checks (D25). The function's `message`, `error`, `log` and panic text are capped, stripped of control (Cc) and format (Cf) characters, NFC-normalised, and go only where D25 says. `message` is shown after the host's preview lines and labelled as the function's (D.4). |
| Nondeterminism between replicas | D2. The commit reuses the planned output and never re-runs the guest. `slice_fuel` cannot change an outcome (A.4). A cross-OS test vector checks exact output and fuel. |

---

## Part B. ABI `oi-fn/1` and the host read model

### B.1 Call protocol

A.4 covers the call. Both documents are UTF-8 JSON. The input is canonical: sorted keys, compact,
and the array orders below, written by `canonical_json` (automate.rs:631-648), which sorts keys
itself and so does not depend on whether serde_json's `preserve_order` is ever unified in. The same
function writes `input_digest`, `edits_digest` and cache keys. The output is parsed with
`serde_json` (recursion limit 128) into types with `deny_unknown_fields`, then canonicalised before
it is hashed or returned. The cache holds the raw output bytes (E.3). One module
may implement several functions: the input names the function in `fn`, and the guest crate's
`entry!` macro dispatches on it.

### B.2 Input manifest (reads that honour visibility)

The manifest is part of the function definition (Part C). It names only aliases (B.5).

```toml
[functions.escalate.input]
# Entity parameters (action and query functions): which aliased properties to include.
params = { issue = ["score", "due"] }
# Action functions only: "exclude" (default) or "include" restricted and private items.
restricted = "exclude"

[functions.escalate.input.walks.blocked]   # becomes data.blocked
from  = "issue"        # an entity parameter, "self" (computed), a find, or another walk
rel   = "blocks"       # relation alias; inverse names resolve as in resolve_rel_lenient
kinds = ["ticket"]     # kind aliases (required, at least one)
open  = true           # optional; or `state = [...]`
depth = 2              # 1..=3
limit = 50             # at most this many visible items in all, after ordering
order = "-score"       # optional; an aliased property, "-" for descending; ties by id
props = ["score", "due"]
on_limit = "cut"       # "cut" (default: keep the first `limit`, more = true) or "error"
```

Query functions may also declare `find` roots:

```toml
[functions.top_risks.input.finds.issues]
kinds = ["ticket"]
open = true
eq = { team = "$team" }   # property equality; a value "$p" is query parameter p, rendered strictly
limit = 200
order = "-score"
props = ["score", "due"]
on_limit = "cut"
```

Rules, checked at compile unless noted:

- **Roots by kind.**
  - Action functions take roots from their parameters only, so the rebuild at commit (D6) walks a
    bounded neighbourhood and never scans a kind.
  - Computed functions take roots from `self` only.
  - Only query functions may `find`.
- **Caps.** The sum of every `limit`, plus parameters and `self`, is at most `caps.entities`.
  Depth is at most 3.
- **Walk order.** A walk collects items breadth first, deduplicated by id, ordered by
  `(depth, order key, id)`, and cut at `limit` for the whole walk, not per node. Each item carries
  `depth` and `parent`.
- **Visibility.** Every candidate passes `see` before it is counted. Hidden items never count, never
  set `more` and never appear as `parent`.
- **Restricted and private items (action functions).** With `restricted = "exclude"`, the default,
  an action function's input is built with the visibility of a plain member of the workspace: an
  entity whose `visibility` is `restricted` or `private` is treated as hidden even when the caller
  may see it, so it is absent, never counts and never sets `more`, exactly as for a reader without
  the grant. An entity **parameter** that is restricted or private is refused, never dropped:
  `function_input: issue:42 is restricted or private, and function escalate reads only public items
  (it would need restricted = "include")`; the caller can see the item, so naming it reveals nothing.
  With `restricted = "include"`, the input is the caller's full view, the definition change is at
  least medium risk (C.5), and any plan whose input holds such an item needs a person's confirmation
  (D.4), so agents and automations refuse it. Computed and query functions always read the caller's
  full view, because their output goes only to that caller.
- **Over the limit.**
  - With `on_limit = "cut"`, `more` is true when more visible items existed than `limit`.
  - With `on_limit = "error"`, the input refuses with `function_input: walk blocked from issue:42
    reached 51 visible items, limit 50`.
- **The scan cap.** The caps above bound what the input holds, not the work: to order and cut a walk,
  the builder examines every incident edge of every frontier node (adjacency is one list per entity
  for all relations, section 2) and checks each candidate's visibility, hidden ones included, and a
  `find` with `eq` examines its whole kind. `limits.scan` (default 20,000, cap `caps.scan` 100,000)
  counts edges and entities examined. The count is a function of the graph and the manifest alone
  (every frontier node is expanded completely, with no early exit), so every replica agrees. Past
  it the input refuses with one fixed text, `function_input: scan budget exceeded`, which does not
  say how much was examined, because that would reveal how much is hidden. The count is recorded in
  `FunctionRun.scanned` (D.2) and in stats and traces for observers (`can_observe`), never in a
  response to the caller. Scanned items are charged to the tenant bucket at `scan_fuel_per_edge`
  (default 50 fuel) after the guard is released, so input building is metered like the guest.
- **Input size.** The canonical input is measured after building. Over `limits.input_kib` it
  refuses with `function_input: input is 300 KiB, limit 256 KiB`, and is never truncated silently.
- **Stored fields only.** Walks and finds read stored fields. Derived and computed names are refused
  at compile, so there is no chaining and no hidden clock.

### B.3 Input document

```json
{
  "abi": "oi-fn/1",
  "call": "action",
  "fn": "escalate",
  "now": 1761696000000,
  "params": {
    "issue": {"id": "issue:42", "kind": "ticket", "title": "Login fails", "state": "open",
              "props": {"due": "2026-11-02", "score": 3}},
    "note": "customer escalation"
  },
  "data": {
    "blocked": {"more": false, "items": [
      {"id": "issue:51", "kind": "ticket", "title": "SSO", "state": "open", "depth": 1,
       "parent": "issue:42", "props": {"score": 2}}
    ]}
  }
}
```

- `call` is `action`, `computed` or `query`. A computed input has `self` instead of `params`.
- **Time.** `now` is present only when the definition declares `uses = ["now:minute" | "now:hour" |
  "now:day"]`, and it is floored to that bucket:
  - for actions, the receipt seed's time (`PlanSeed.now`);
  - for queries, the call time;
  - for computed functions, the read time.

  Phase 2 has no unbucketed `now`. This keeps preview and confirmation equal within a bucket (D11)
  and caches shareable.
- `actor` is present only with `uses = ["actor"]`, and never for computed functions, so their cache
  entries can be shared by readers who see the same data.
- `kind` is the kind alias the walk matched the item by (the first matching alias in its `kinds`).
  `props` holds only the declared aliases that are set; a missing property is absent, not `null`.
- `id` values are opaque. A kind rename changes their prefix (`issue:42` becomes `ticket:42`, the
  old id redirecting), so a guest must never parse one, and codegen offers no kind-prefix helper
  (D9). Recorded inputs (`--record`) are re-recorded after a kind rename.
- Created ids, `new_id` and the receipt id are never in the input (B.4). A plan and a later apply
  over unchanged data therefore have the same input and share one cached output.

### B.4 Output document: edits returned, not applied

```json
{
  "edits": [
    {"patch":  {"id": "issue:42", "state": "escalated", "props": {"score": 5, "level": "p1"}}},
    {"unlink": {"from": "issue:42", "rel": "owner", "to": "person:ana"}},
    {"link":   {"from": "issue:42", "rel": "owner", "to": "person:kenji"}},
    {"create": {"kind": "followup", "title": "Call customer", "props": {"due": "2026-11-01"},
                "links": [{"rel": "about", "to": "issue:42"}]}},
    {"delete": "issue:77"},
    {"event":  {"name": "escalated", "target": "issue:42"}}
  ],
  "message": "Escalated to Kenji",
  "log": ["raised 1 issue"]
}
```

- The document holds exactly one of `edits` (action), `value` (computed, query) or `error` (any). It
  may also hold `message` (actions only, ≤ 256 characters, shown with the plan) and `log` (≤ 4 KiB,
  kept in traces only). Unknown keys are refused.
- **Empty edits.** `edits: []` is a valid output meaning "nothing to change". For an interactive
  apply it gives the existing invalid plan with the warning "nothing to change"
  (action_types.rs:1312-1315), so a rejected receipt and a failure event. For an automation it is
  `NotMet`: no row and no receipt (D.6), which is the normal case for a materialisation whose value
  did not change.
- **Ids.**
  - The allowed targets are the `id` fields of the entity objects in the host-built input: entity
    parameters, `self`, and walk and find items. An id that appears only in `actor`, `parent`, a
    property value or a text parameter is not a target. Every `id`, `from`, `to`, `target` and
    `delete` must be an allowed target, with no exception for well-known ids, and `unlink.to` must
    be one too, so a function can only remove links whose other end it read.
  - `create` has no `id`. The host assigns `{kind}:{new_id}-{n}`, with `new_id` from the receipt
    seed (`PlanSeed`, action_types.rs:507-525) and `n` the create's index among the output's
    creates. A re-plan with the same receipt therefore makes the same ids.
  - A created object is connected only through its own `links`, and no other edit may name it.
  - If an assigned id already exists, the plan is refused (a plan check in `plan_effects`, D.2,
    never a cached output failure). It is never turned into an irreversible update, unlike
    templated creates (1232-1235).
- **Edits are not repeated or vague.** A second `patch` or `delete` of the same id is refused
  ("merge the patches"). `unlink` names `to` explicitly; there is no `*`. **There is no `replace`**:
  the native planner would expand it over every existing link of that relation on the raw graph,
  hidden ends included, after `edits_digest` was taken (action_types.rs:1134-1151). A function that
  replaces an owner reads the current owners through a declared walk and returns explicit `unlink`s.
- **Names.** Names are aliases (B.5) and must be in the `writes` envelope (C.3). `event.name`
  becomes `fn.<function>.<name>` and must be one of `writes.events`.
- **Events carry no guest payload.** An `event` edit is `{name, target}`. The host writes the
  payload, `{"function": name, "artifact": digest}`. On main, readers who cannot read the target
  never see the event (`a10d5b6`), and the target is an input id. Typed payloads are deferred
  (section 9): a typed value is still chosen by the guest, so it needs a rule tying it to the
  target's own properties first.
- **Values are literal.** The host never renders placeholders in them (D7). Property values are
  validated against the schema when they are staged (engine.rs:1641), as for every write.
- **Text.** `message`, `error` and `log` lines are capped, stripped of control (Cc) and format (Cf)
  characters (bidi overrides U+202A-202E and U+2066-2069, zero-width characters), and NFC-normalised
  (`unicode-normalization` and `unicode-properties` are already in the lock). They go only where
  D25 allows.
- **Violations.** Every violation is listed, sorted, in one failed plan check `function`. The text
  is the host's: the rule and the edit index or path (`function_output: edits[3].id is not an input
  entity; edits[5].props: alias level is not in writes.patch.ticket`), never a string the guest
  produced, since a guest-chosen id could carry anything. The full detail, guest strings included,
  goes to the receipt's function block and to traces with `capture_content`. Nothing is skipped and
  nothing is partly applied.
- **`edits_digest`** is `sha256(canonical(edits))`, the returned edits before ids are assigned
  (D11). With no `replace`, the planned mutations are exactly these edits, so the digest covers
  everything that commits. `output_digest` covers the whole canonical document.

### B.5 Names and aliases

```toml
[functions.escalate.names]
kinds = { ticket = "issue", person = "person", followup = "task" }
rels  = { blocks = "blocks", owner = "assigned_to", about = "about" }
props = { score = "issue.risk_score", due = "issue.due", level = "issue.escalation" }
```

- Aliases pass `valid_name` (`[a-z][a-z0-9_]*`, live_schema.rs:929). Targets must exist:
  - kinds and interfaces;
  - relations, with inverse names allowed;
  - qualified `kind.property` names, where the kind may be a subtype or an interface that has the
    property.
- Renames rewrite the right-hand sides through the existing passes: `rewrite_kind` (1264),
  `rewrite_rel` (1358) and the `RenameProperty` arm of `apply_one` (1067). That arm computes the set
  of kinds that `kind_is` the renamed type (1094); the functions pass rewrites every `names.props`
  target whose kind is in that set, and every interface-qualified target the property comes from,
  so renaming `issue.risk_score` also rewrites `subissue.risk_score`. Aliases never change, so
  compiled code keeps working. Entity ids are another matter: a kind rename changes their prefix
  (B.3).
- `state` and `title` are not aliases. They are fields of every entity.

### B.6 Determinism rules

1. No imports, so there is no clock, randomness, environment or I/O.
2. Inputs are canonical. Keys are sorted by `canonical_json`, never by relying on `serde_json::Map`
   being a `BTreeMap`, and arrays are in the stated order, never in `Graph` iteration order
   (graph.rs:18-21). A unit test inserts keys out of order and asserts sorted canonical bytes.
3. Fuel is the only limit, and instantiation is charged in it. Compilation is eager. Floats follow
   the deterministic profile. Slicing does not change fuel or outcome, grows included (A.4).
4. The output is canonicalised before it is hashed. Edit order is the function's and is kept.
5. The commit never re-runs the guest (D6). A computed value is recomputed only on a cache miss, and
   a miss with the same input gives the same bytes. The scan count is a function of the graph and
   the manifest (B.2), so a scan refusal is the same on every replica.
6. wasmi is pinned exactly. A wasmi upgrade may change fuel costs, and the release notes say so. A
   plan made on a replica of one version commits on a replica of another unchanged, because the
   output is recorded. The engine id in cache keys keeps cached faults from crossing versions.
7. A test vector, a fixture module and input with the expected output digest and exact fuel, runs
   in `cargo test --workspace` on Linux, macOS and Windows.

### B.7 Failure classes (one table for every surface)

The "Action plan" column is the host-written check text, which is also what reaches the
`action.<name>` failure event and a run's reason (D25). Guest text (an `error`, a panic message, an
id the guest invented) is never in it; it goes to the caller's response, the receipt's function
block and traces with `capture_content`. "Cached" refers to the sandbox-result cache (E.3), which
holds only what the sandbox produced; every other check runs again on each use.

| Class | Cause | Action plan (host text) | Query, `function.test` (HTTP) | Computed | Cached | Retried by runs |
|---|---|---|---|---|---|---|
| fuel, memory, stack | limits reached (instantiation included) | failed check `function_limit: escalate ran out of fuel (50M; input sha256:…)` | `Invalid` (400) | `computed_errors` | yes (the fault) | no |
| trap | unreachable, out of bounds | `function_trap: escalate: <code> (input sha256:…, fuel …)` | `Invalid` (400) | same | yes (the fault) | no |
| guest error | the function returned `error`, or panicked with `oi_panic` exported | `function_error: escalate returned an error (input sha256:…, fuel …)` | `Invalid` (400), with the guest's text for the caller | same, with the text | yes (the raw output, or the fault) | no |
| output (sandbox) | malformed JSON, over the output cap, pointer out of bounds | `function_output: escalate: <rule>` | `Invalid` (400) | same | yes (the raw bytes or the fault) | no |
| output (rules) | outside the envelope, an unknown alias, an id that is not an input entity, a type mismatch | `function_output: edits[3].id is not an input entity; …` (rules and indexes) | `Invalid` (400), with paths | same | no: checked on every use, after the cache | no |
| created id exists | an assigned id is already taken | `function: created id followup:… exists` (a plan check) | — | — | no | no |
| input | `on_limit = "error"` reached, input bytes over the cap, a restricted parameter | `function_input: …` | `Invalid` (400) | same | no (no guest ran) | no |
| scan | `limits.scan` exceeded while building the input | `function_input: scan budget exceeded` (fixed text) | `Invalid` (400) | same | no | no |
| artefact | missing, unverified for this tenant, or corrupt bytes | `function_artifact_missing` / `_corrupt` | `Invalid` (400) | same | no | no |
| declared limit above cap | a cap was lowered | `function_limit: declared fuel … exceeds the server cap …` | `Invalid` (400) | same | no | no |
| definition changed | the action type's function or its definition hash changed between prepare and commit | after 3 attempts the receipt is `rejected`: `inputs_changed: function escalate changed (sha256:a… → sha256:b…)` | — | — | n/a | yes (`Conflict("inputs_changed: …")`) |
| off | process off, operator off, tenant kill, function off, digest denylisted or quarantined (also at a slice) | failed check `functions_off: <which, by whom>` | `Unavailable` (503) | same | no | no: the run is `skipped` and the automation paused once (D.6) |
| internal | the interpreter panicked | `Unavailable("function_internal: …")` | 503 | same | **never** | yes (counted toward quarantine) |
| capacity | no slot within `queue_ms`, or a single-flight leader not done within `leader_wait_ms` | `Unavailable("function_capacity: …")` | 503 | same | no | yes |
| stopping | shutdown, or cut at the deadline | `Unavailable("stopping: …")` | 503 | same | no | yes (the next holder) |
| budget | tenant bucket empty, the principal's share spent, or a reserve reached | `Limited("functions: … retry in N s")` | 429 with the true `Retry-After` | same | no | **yes, at the refill time (new rule, D.6)** |
| inputs changed | the commit-time input digest differs | after 3 attempts the receipt is `rejected`: `inputs_changed: …` | — | — | n/a | yes (`Conflict("inputs_changed: …")`) |
| edits changed | a confirmed plan's `edits_digest` differs from `expect` | the receipt is `rejected`: `precondition_changed: …` | — | — | n/a | n/a (runs never confirm) |

Deterministic classes are plan checks for actions, so `apply_action` returns a `rejected` receipt.
For automations, a deterministic class found at the pre-claim evaluation gives a `refused` row
(`plan_invalid: …`, onto-intelli automate.rs:459-472), and one found at commit gives a `failed` row.
Only transient classes are `Err` values. An interpreter panic never takes a deterministic class: it
is transient for the caller and a quarantine signal for the operator (I.2).

---

## Part C. Artefacts, schema section, versioning, governance

### C.1 Artefact store (`crates/onto-core/migrations/0015_function_artifacts.sql`)

```sql
-- Function modules (docs/design/FUNCTIONS.md C.1): WASM bytes by content address. The schema names
-- digests only; rows are inserted once (ON CONFLICT DO NOTHING) and never updated or deleted.
CREATE TABLE IF NOT EXISTS {schema}.function_artifacts (
    digest TEXT PRIMARY KEY CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    bytes BYTEA NOT NULL,
    size INT NOT NULL,
    info JSONB NOT NULL,          -- ModuleInfo (A.3), plus an optional source {url, digest}
    created_at BIGINT NOT NULL,   -- {schema}.now_ms()
    created_by TEXT NOT NULL
);
DROP TRIGGER IF EXISTS function_artifacts_append_only ON {schema}.function_artifacts;
CREATE TRIGGER function_artifacts_append_only BEFORE UPDATE ON {schema}.function_artifacts
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS function_artifacts_no_delete ON {schema}.function_artifacts;
CREATE TRIGGER function_artifacts_no_delete BEFORE DELETE ON {schema}.function_artifacts
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_delete();
DROP TRIGGER IF EXISTS function_artifacts_no_truncate ON {schema}.function_artifacts;
CREATE TRIGGER function_artifacts_no_truncate BEFORE TRUNCATE ON {schema}.function_artifacts
    FOR EACH STATEMENT EXECUTE FUNCTION {schema}.refuse_delete();
```

- The migration is additive like 0009-0014 and is appended to `MIGRATIONS` (postgres.rs:70-85).
  Older builds never read the table.
- `Store` gains `artifact(digest) -> Option<ArtifactRow>` and `artifact_bytes() -> u64` (for the
  quota). `WriteBatch` gains `artifacts` and `min_writer: Option<i32>` (C.9). `MemoryStore`
  implements the same behaviour.
- Artefacts are never loaded during catch-up. They are fetched on first use and cached (A.7).
- **No garbage collection in phase 2.** Every schema version is kept (live_schema.rs:745-772), and
  revert or undo may need any artefact a version named. Uploads that nothing ever references count
  against the quota. A later retention sweep can delete them through the existing `onto.retention`
  escape of `refuse_delete` (0008:7-13), with no migration change (section 9).
- **Operator purge.** A module's data segments can hold any bytes, including personal data copied
  from the graph or content that must be taken down, so there is one way to delete a row: the hub
  admin action `functions.purge {tenant, digest, reason}` (operator credentials only, never a
  tenant principal). In one transaction it sets `onto.retention` locally, deletes the row, and
  records a `function.artifact.purged` event through the engine; every replica then drops the
  digest from its module cache and from that tenant's `verified` set at the next
  `control_poll_ms`. Schema versions that name the digest still open, and calls refuse with
  `function_artifact_missing`. The runbook in STORAGE.md adds the digest to the denylist file
  (I.2), so the same bytes cannot be uploaded and loaded again.

### C.2 Upload (`function.upload`)

1. The principal's upload rate is checked (`uploads_per_minute`, default 10 per principal).
2. The digest is `sha256` of the bytes (`sha2` is already a dependency, Cargo.toml:73). A digest
   that is denylisted is refused. A digest this tenant already stores returns at once, with no
   validation and nothing added.
3. `onto-sandbox::compile` checks the rules (A.3). The bytes are never run: there is no `describe`
   call and no code at upload.
4. The tenant quota (`artifact_quota_mb`, default 256) is checked, and for agents also a
   per-principal quota (`agent_artifact_quota_mb`, default 32).
5. One `transact` commits the row (`Extra.artifacts`, `INSERT … ON CONFLICT (digest) DO NOTHING`)
   together with a `function.artifact` audit event naming the digest, size and uploader.

An uploaded artefact does nothing until a governed schema change names its digest. Uploads are
refused while the tenant kill or the operator switch is off.

Routes:

- HTTP `PUT /v1/functions/artifacts` takes a raw `application/wasm` body, with its own
  `DefaultBodyLimit::max(caps.module_kib)` on that route only (the console's pattern,
  onto-grpc console.rs:49, 86).
- gRPC `FunctionService/Upload {bytes module}` raises `max_decoding_message_size` to the module cap
  for that service only.
- Upload bodies are never traced.

### C.3 Schema section `[functions.<name>]`

```rust
// crates/onto-core/src/functions/def.rs
#[serde(deny_unknown_fields)]
pub struct FunctionDef {
    pub kind: FunctionKind,                            // action | computed | query
    pub artifact: String,                              // "sha256:<64 hex>"
    #[serde(default = "abi_v1")] pub abi: String,      // "oi-fn/1" (phase 2.1 adds "oi-wasi/1")
    #[serde(default)] pub description: String,
    #[serde(default)] pub source: Option<Source>,      // {url, digest}; shown to reviewers, "unverified"
    #[serde(default)] pub uses: Vec<String>,           // now:minute|hour|day, actor
    #[serde(default)] pub params: BTreeMap<String, ParamDef>, // query only (actions: the action type's)
    pub names: Names,                                  // kinds, rels, props (B.5)
    #[serde(default)] pub input: InputSpec,            // B.2, including `restricted` (actions only)
    #[serde(default)] pub output: Option<TypeDef>,     // computed (scalar, required), query (default json)
    #[serde(default)] pub writes: Option<Writes>,      // action only
    #[serde(default)] pub limits: FnLimits,            // fuel, memory_mb, input_kib, output_kib, scan;
                                                       // within the caps; omitted fields take defaults
}
pub struct Writes {
    pub patch: BTreeMap<String, Vec<String>>,   // kind alias -> prop aliases, plus "state" | "title"
    pub link: Vec<String>, pub unlink: Vec<String>, // relation aliases
    pub create: BTreeMap<String, Vec<String>>,  // kind alias -> props settable on create
    pub delete: Vec<String>,                    // kind aliases
    pub events: Vec<String>,                    // suffixes of fn.<name>.<suffix>
    pub max: usize,                             // ≤ caps.edits
}
```

- **`TypeDef`** is a scalar `PropType` with `PropertyDef` bounds (schema.rs:62-73, 131), or
  `entity {kinds}`, `list {items, max}`, `object {fields, optional}` or `json`. A computed output
  must be a scalar checked with `PropertyDef::accepts`. Every output is validated and refused with
  a path (`function_output: $[3].priority: expected int, got "high"`). An `entity` value must be the
  id of an object in the input.
- **Where the fields go.**
  - `Schema.functions: BTreeMap<String, FunctionDef>` with
    `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`, as for `automations`
    (schema.rs:528-531).
  - The same field goes on `SchemaFile`; `extend_toml` replaces a function by name.
  - `ActionType.function: Option<String>` and `ObjectType.computed: BTreeMap<String, ComputedDef
    {function}>` both skip serialising when empty. Schemas, action types and object types without
    functions therefore serialise byte for byte as today, and existing fingerprints and closure
    hashes do not change.
  - `deny_unknown_fields` goes on the new structs only. Putting it on existing structs would
    retroactively refuse stored schemas.
- **`compile()` checks structure and references only**, like automations (schema.rs:798-801):
  - names are valid and every alias target exists;
  - `writes` and `input` name only aliases;
  - **`writes` never touches access control**: a `patch` or `create` alias whose target is a
    `visibility` or `acl` property, or a `link` or `unlink` alias whose target is in `ACL_LINK_TYPES`
    (`member_of`, policy.rs:315), is refused (`function_writes: function escalate may not write acl,
    visibility or member_of; access control stays with people`). Reading them is allowed;
  - `restricted` is set only on action functions;
  - roots fit the kind (B.2), and the entity sum and depth are within the caps;
  - `output` is present and scalar for computed functions;
  - each referencing action type declares every parameter the input names, with the right type, and
    each `params` binding names an entity parameter;
  - for computed functions, the referencing object type (or an interface it implements) is in
    `names.kinds`, and the computed name collides with no stored, derived or interface property
    (`computed_name_conflict`);
  - deleting a function that an action type or a computed property names fails compile, naming
    the users.
- Whether the artefact exists, and whether limits fit the caps, is checked at plan, propose and
  apply (C.5), never at load. A stored schema therefore always opens, and a call with a missing
  artefact refuses loudly.

### C.4 Live schema operations

`SchemaChange::PutFunction { name, def }` and `DeleteFunction { name }`. Every hand-written list gets
an arm in the same step:

- `op()` and `target()` (live_schema.rs:845, 875);
- `apply_one` (1025);
- `sections()` (1458) and `diff` (1513);
- **`changes_between` (1554-1600)**, whose missing arm would otherwise turn a function into a
  glossary term (1583-1596);
- `summarize` (2545);
- the undo "still present" match (3852-3868);
- `SchemaStatus` counts (822);
- `locks::SECTIONS` and the touched mapping (locks.rs:48-58, 284-294);
- the pack layer names (packs.rs:404), with collisions checked;
- the three rename rewriters (B.5), with `RenameProperty` covering subtype and interface targets.

The automation closure (`closure()`, automate.rs:686-725) gains, for every reached action type with
`function = f`, an item `function:f = hash_of(FunctionDef)`. That covers the digest, names, input,
writes and limits, but not the engine id (A.2). The same hash is `FunctionRun.def_hash`, which the
commit checks for interactive applies (D.2). Since the field is absent elsewhere, existing closures
hash as before. `closure_findings`, `stale()` (935) and `stale_at_commit` (runs.rs:498-511) pick the
item up unchanged, so a code swap pauses enabled automations until a person enables them again.

### C.5 Plan checks, risk and previews

- **`check_functions`** runs next to `check_automations` (live_schema.rs:3559), in `schema_plan_as`
  (3497) and `schema_commit` (3624), in two parts:
  - **Before the lock**, keyed by digest through the module cache, the way `early` hoists the
    consequences (3638-3641): the artefact is stored and verified for this tenant, and its `info`
    passes today's rules (re-validated with `compile` once per digest per process, so a tightened
    rule shows up in plans). Rows are append-only, and only an operator purge removes one; a purge
    racing the commit leaves a schema naming a missing artefact, which is already a state the design
    handles (C.10). Nothing touches module bytes under the write lock.
  - **Inside the lock**, only pure checks on the change: declared limits within the caps, the
    module's initial memory within `limits.memory_mb`, and the count against `max_functions`, like
    `check_limit(base, new, …)` (3561).
  - The refusals apply **only to functions that the change set puts or newly references** (an
    action type or computed property pointed at them). A pre-existing violation (a lowered cap, a
    missing artefact in config mode) is a plan warning, so deletes, the rollback steps (C.9) and
    unrelated schema changes still apply.
- **Risk** (`assess_risk`, 3146) is at least **medium** when a change replaces the artifact,
  `names`, `input`, `writes` or `limits` of a function that an action type, computed property or
  enabled automation reaches, or sets `restricted = "include"`. The reason names them ("replaces
  code of function escalate, reached by action type escalate and automation triage_new; the
  automation stops until re-enabled"). A breaking signature change (C.6) is also medium.
- **Function preview** (`SchemaPlan.functions`, built in the `plan_shapes` pattern):
  - inputs are built under the plan's read guard, and the guests run after `drop(g)`
    (live_schema.rs:3510-3527);
  - for each new or changed **computed** function, old and new outputs are shown on up to 20
    visible subjects of its kind (in id order), run as the planner;
  - for a **query** function without required parameters, there is one call old against new;
  - otherwise it says `skipped: <why>` ("query parameters are required; run function.test",
    "action functions are previewed with function.test");
  - the total is bounded by `preview_fuel` (default 200M), charged to the requester's share of the
    tenant bucket as a read (I.1).
- **Previews are per reader and never stored.** The preview names subject ids and computed values
  as the planner could see them, so `without_reader_facts` (live_schema.rs:586-596) replaces
  `functions` with `skipped: computed for each reader` before a proposal is stored, and
  `with_reader_view` (600-607) restores it from a fresh plan. `list_proposals` returns the skipped
  form. `get_proposal` (and `ProposalView.preview`, proposals.rs:118-129) computes it as the viewer,
  within the viewer's share, **only when asked** (`preview_functions: true`), because a proposal's
  new code is unapproved and may come from an agent; the same tenant setting that governs drafts
  (C.7) decides who may ask. Results are cached per (proposal id, proposal revision, `scope_key` of
  the viewer), so reloading a proposal does not re-run it.
- **Review aid.** The plan and `ProposalView` show the digest, size, exports, limits, `names`,
  `input` and `writes` as a structured diff against the previous definition, plus `source` marked
  "unverified" (reproducible builds are deferred).

### C.6 Versioning and signatures

- A function's identity is its digest. There is no semver, no auto-upgrade and no version range.
- Callers name the function, and the name resolves to the head schema's definition. Receipts,
  traces and query responses carry the digest, and a function's "version" is the schema version
  that put it.
- **Signature** = `sha256(canonical {kind, params, output})` for query and computed functions. It
  is shown by `function.get` and checked by `expect_signature` on `function.call`
  (`function_signature_changed: top_risks changed from sha256:… to sha256:…`).
- A change that alters it adds the plan finding `function_breaking_change` (warning, medium risk).
  The docs advise a new name (`top_risks_v2`) for apps that cannot move together.
- `abi` is `oi-fn/1` in phase 2. A future rule set is a new value, selected per definition.

### C.7 Who may do what

| Operation | Who |
|---|---|
| `function.list`, `function.get`, `function.types`, `function.artifact` (info) | anyone who can read the schema ("function definitions are visible to everyone who can read the schema") |
| `function.upload` | writers (member, admin). **Agents: yes** (owner decision 3: the bytes are inert, and agents need them to propose), within the tenant quota, a per-agent quota and a per-principal upload rate |
| `put_function`, `delete_function` | schema governance: applying needs `can_apply`, approving needs `can_approve`, never agents. Agents and members propose |
| `function.call` (query), `function.input` | any principal with read, as itself |
| Computed values on read | whoever can see the entity, as itself, when asked for |
| A function-backed action | the action type's `roles`, as today. Agents plan and apply only low-risk, reversible plans (action_types.rs:1482-1488) |
| `function.test` on a published function or a fixture | any principal with read, agents included; read-only, budgeted |
| `function.test` on a draft (an uploaded artefact with an unpublished definition), and function previews on a proposal | by the project setting `functions.drafts`: `"people"` (default: principals that may write, except agents), `"writers"` (agents too) or `"off"` |
| `function.stats` | `can_observe`, as for traces |
| `function.disable`, `function.enable` (one function) | tenant admins; never agents. Audited through `commit_config` |
| `function.kill`, `function.resume` (tenant) | tenant admins; never agents. Audited through `commit_config`. `function.resume` refuses with `functions_off: operator` while the operator switch is off |
| Operator switch (`functions.enabled` in project settings), the denylist file, `functions.purge` | the operator only, through the control plane or the hub's admin credentials; never a tenant principal |

There is no `run_as`, so the dominance check (EVENTS.md 6.1) does not apply: the caller's own rights
bound everything. Refused configuration acts go through `refuse_configure` (outbox.rs:398) and write
an audit-denied event, as in phase 1.

### C.8 Lint (new `CHECKS` rows, lint.rs:104)

| Check | Severity | Finding |
|---|---|---|
| `function_reference` | error | An action type, computed property or automation names an unknown function, or one of the wrong kind. |
| `computed_name_conflict` | error | A computed name equals a stored, derived or interface property. |
| `function_artifact_missing` | error | The artefact is not stored. It needs store state, and is reported in `not_run` without it (lint.rs:501-519). |
| `function_writes` | warning | The envelope allows `delete`, or `max` is at the cap. Automations that reach it are refused when the output deletes. (An envelope that names access control is a compile error, C.3.) |
| `function_input_wide` | warning | On current data, a computed function's walk is already cut, would error with `on_limit = "error"`, or exceeds `limits.scan` for some of up to 1,000 sampled subjects. It reports counts only ("cut for 37 of 1,000 sampled subjects"), never subject ids, so the lint stored with a proposal names nothing the planner alone could see. It needs a graph. |
| `computed_trigger_blind` | warning | An automation trigger or webhook filter names a computed property, which the change feed never carries. |
| `function_limits` | info | A declared limit is at or above 80% of its cap. |
| `function_time` | info | `uses` includes a `now` bucket. |
| `function_unused` | info | An action or computed function referenced by no action type or computed property. Query functions are excluded: apps call them by name, so the schema never references them. |

`action_writes` (lint.rs:1757) reads the `writes` envelope of function-backed action types, so
`automation_loop` (1902, a warning) reports cycles through functions, with the note that an envelope
is an upper bound, and the privilege checks keep working. No separate self-trigger check is needed:
an automation never triggers itself unless `allow_self` is set (automate.rs:353-357), and
`automation_loop` already reports that case (lint.rs:1910-1915).

### C.9 Writer fence and rollback

- This build sets `WRITER_VERSION = 2` (postgres.rs:92).
- Every schema commit whose resulting schema holds a function, a function-backed action type or a
  computed property sets `WriteBatch.min_writer = Some(2)`. PgStore writes
  `UPDATE head SET min_writer = GREATEST(min_writer, $1)` in the same transaction, which is
  idempotent. This covers `schema_commit` and config-mode imports (`record_version`,
  live_schema.rs:3425).
- **What a phase-1 replica does.** In the common case it cannot read the tenant at all. Its
  `SchemaChange` enum is closed with no catch-all (live_schema.rs:142-143), so it cannot
  deserialize a head version whose `changes` hold `put_function` or `delete_function`; and its
  `validate` requires an effect (action_types.rs:283-286), so it cannot compile a schema holding a
  function-backed action type. `read_version` maps either failure to a storage error (3261-3267),
  catch-up propagates it (engine_sync.rs:107-110), reads that sync first fail with it (engine.rs:
  705-712), and a restart refuses the tenant. It fails loudly, never silently.
- **What the fence adds.** The remaining case is a head version that is a later ordinary change,
  with no function-backed action type, which the old build could reload without the `functions`
  section and commit. There the replica is refused on every state-changing write with
  `Unavailable("tenant requires a newer onto-intelli …")` (postgres.rs:672-683).
- Builds older than phase 1 cannot open tenants at migration ≥ 9 at all (postgres.rs:798-808).
- **Deploy order is a hard requirement.** Every replica runs this build before the first
  `put_function`. There is no replica registry from which the hub could refuse `put_function` while
  older writers are live, so STORAGE.md states the requirement next to the phase-1 rollout notes,
  and docs/FUNCTIONS.md repeats it before the first example.
- A test pins the loud failure: a version whose `changes` hold an op this build does not know
  (`{"op": "put_widget"}`) fails catch-up and `reader` with a storage error naming the version, and
  never loads a partial schema.
- **Rollback** (STORAGE.md gains these steps, after the phase-1 ones at 122-136):
  1. Delete every function-backed action type (or put it back with effects), every computed
     property and every function.
  2. Apply one more ordinary schema change, so the head version's `changes` hold only operations
     the older build knows.
  3. As the operator, run `UPDATE head SET min_writer = 1`. This is deliberately manual.
  4. Delete the `schema_migrations` rows with `version >= 15`. The `function_artifacts` table
     stays, and older builds never read it.

### C.10 Config mode and packs

- Config mode refuses API schema edits (live_schema.rs:3594). There, the TOML names digests, and the
  operator uploads artefacts with `onto fn upload`, which is not a schema edit and works in every
  mode.
- A schema whose artefact is missing still opens, and calls refuse with `function_artifact_missing`
  on every replica, even one whose module cache holds the same digest for another tenant (A.7).
- Packs do not ship functions in phase 2: pack TOML is `include_str!` text with no bytes.

---

## Part D. Function-backed actions

### D.1 Declaration

```toml
[action_types.escalate]
function = "escalate"          # instead of effects
risk = "low"                   # a floor; the output can only raise it
roles = ["member"]
parameters = { issue = { type = "entity", kinds = ["issue"] }, note = { type = "text" } }
preconditions = [ { prop = "issue.state", ne = "closed" } ]
```

`validate` (action_types.rs:283) accepts exactly one of non-empty `effects` or `function`, and
`function` must name a function of kind `action`. With `function` set, `unattended` must be empty,
and `primary_param` (270) returns the first entity parameter in name order.

### D.2 Prepare outside the lock, plan natively

`PlanSeed` gains `function: Option<Arc<FunctionRun>>`, everything a re-plan needs to reproduce the
first plan:

```rust
pub struct FunctionRun {
    pub name: String, pub artifact: String, pub engine: String,
    pub def_hash: String,                        // hash_of(FunctionDef), the closure item (C.4)
    pub revision: u64,                           // Ontology::revision() when the input was built
    pub scanned: u64,                            // edges and candidates examined (B.2)
    pub input_digest: String, pub edits_digest: String,
    pub outcome: Result<Vec<FnEdit>, FnFailure>, // decoded edits (aliases resolved, envelope checked),
                                                 // or a deterministic failure (B.7) for the plan check
    pub detail: Option<String>,                  // guest text (error, panic); receipt and traces only
    pub message: Option<String>, pub fuel: u64, pub cached: bool, pub more: Vec<String>,
    pub restricted_read: bool,                   // the input held a restricted or private item
}
```

```text
prepare(ctx, action, params, seed, also) -> Result<PlanOrRun>                    // never under a guard
  (pre, input) = self.read(|g, schema| {
      pre = pre_plan(g, schema, ctx.principal, at, params, also)   // roles, write right, parameter
      if !pre.ok { return (pre, None) }                            // normalisation, preconditions
      (pre, Some(build_input(g, schema, ctx.principal, also, fdef, params, seed)))   // scan-capped
  })
  pre failed -> return the ordinary failed plan: no guest runs and no fuel is spent
  charge scanned edges; switches, slots, budget (RAII, A.6)
  raw = runtime.run_cached(fdef, input)                             // Err only for transient classes
  decode(raw) -> FnEdits | FnFailure                                // B.4 rules on every use (E.3)

plan_on_as(g, …, seed) for a function-backed type:
  1. pre_plan exactly as today: roles, the write right, parameters, preconditions
  2. seed.function is None -> failed check `function: not prepared` (an internal error, tested)
  3. at.function != run.name, or hash_of(current def) != run.def_hash
       -> Err(Conflict("inputs_changed: function escalate changed (sha256:a… → sha256:b…)"))
     a switch is now off (process, operator, kill, function off, denylist) -> failed check
       `functions_off: …` (final)
  4. the ontology's revision() == run.revision -> the input is unchanged, skip to 5
     else build_input(g, …, scan limit = min(limits.scan, 2 × run.scanned + 1,000));
       over the limit or digest != run.input_digest -> Err(Conflict("inputs_changed: …"))
  5. run.outcome is a failure -> failed check `function` with the host text (B.7); the guest text
     stays in run.detail
  6. run.restricted_read -> needs confirmation, reason "function escalate read restricted or
     private items" (B.2)
  7. turn each FnEdit into an Effect (literal strings; create key "<new_id>-<n>")
  8. plan_effects(g, schema, env, effects, acc) with Templates::literal(…)
```

- **The refactor comes first, with no behaviour change.** The per-effect body of `plan_on_as`
  (1065-1310) becomes `plan_effects`, and the checks before it (roles and the write right at
  956-966, parameter normalisation at 951, preconditions at 1061-1064) become `pre_plan`, so
  `prepare` can run them before any guest. `Templates` gains a `literal` flag: `render`,
  `render_props`, `render_value` and `render_strict` (609-640) return their input unchanged when it
  is set. In function mode, `plan_effects` refuses a create whose id exists (B.4) instead of warning
  (1232-1235). Templated action types take exactly the old path, and the existing action-type tests
  are the gate.
- **Order of checks.** Because `pre_plan` runs first, a reader without the action type's role who
  calls `action.plan` (a read, admitted for any reader, onto-intelli actions.rs:178-181, 928-972)
  gets the ordinary failed plan and spends no fuel, and a failed precondition never runs the guest.
- `apply_action` (1434), `plan_action` (914) and `plan_action_consequences` (1396) call `prepare`,
  then plan. On `inputs_changed` they prepare again, at most 3 attempts in all. After that the
  receipt is `rejected` with `inputs_changed: the data escalate reads changed while planning; plan
  again`. The receipt id, and so the seed, stays the same across attempts, so created ids are
  stable.
- `ActionPlan` (405-432) gains `function: Option<FunctionPlan>` with `skip_serializing_if`. Its
  fields are `{name, artifact, engine, def_hash, input_digest, edits_digest, fuel, cached, more,
  message, detail}`. The plan goes to the caller and into the receipt (its actor and admins), so
  `message` and `detail` (the guest's text) stay there and never reach an event (D25). The decoded
  edits stay in memory (`PlanSeed.function`). They are not stored twice: the plan's `mutations` and
  `inverse` already hold them.

### D.3 Commit: the input digest check

`apply_planned` (1503) gains a parameter `function: Option<Arc<FunctionRun>>` and builds its seed
as `PlanSeed::of(id, ts)` plus that run. The three existing test callers pass `None`. Inside
`transact`, `plan_on` first checks the definition (D.2 step 3), then the input:

- **Nothing moved** (`revision()` equals the revision recorded at `prepare`): the input is the one
  the guest saw, so no rebuild runs. Catch-ups bump the revision too (engine_sync.rs:120-133), so
  this is safe. This is the common case and costs nothing under the lock.
- **Something moved:** the input is rebuilt on the locked graph with the scan limit
  `min(limits.scan, 2 × run.scanned + 1,000)`, so the work under the lock is bounded by what was
  charged at `prepare`.
  - **Equal digest:** the recorded edits are planned natively on the locked graph, so the inverse,
    `touched`, risk and the "already matches" notes are fresh. Then `drift()` runs unchanged.
  - **Different digest, or over the scan limit:** `Err(Conflict("inputs_changed: …"))`. Nothing is
    written, and `apply_action` goes back to `prepare` (counted in the 3 attempts).
- **Definition changed** (another function, another definition hash): the same `inputs_changed`
  conflict, so `prepare` runs the code now in force. A confirmation bound with `expect` then
  refuses if the new code returns different edits (D.4). A switch now off is a final
  `functions_off` check.
- A `Stale` retry inside `transact` (engine_sync.rs:54) moves the revision, so it rebuilds the input
  again, still under the same scan limit. That is native and bounded, never the guest.

The worst-case cost under the write lock is one input build of at most `2 × scanned + 1,000` items
examined (and at most `caps.scan`) plus one SHA-256 over at most 1 MiB. Step 16 measures it, with
a target of p99 < 2 ms at the default caps, and benchmarks a hub with 1M links and a principal with
100k `member_of` links (section 6).

### D.4 Preview, risk and confirmation bound to edits

- Preview lines are the existing ones (`patch issue:42 …`, `create task:… "Call customer"`,
  `record event fn.escalate.escalated on issue:42`). The UI adds "Computed by function escalate
  (sha256:9f2c…)".
- **The function's `message`** (sanitised, B.4) is rendered after the preview lines, in a distinct
  style, labelled "Text from function escalate, not checked by onto-intelli". It never appears in a
  dialog title or among the host's preview lines, so it cannot pose as one.
- **Risk** is the declared floor, raised to High by any delete (1277) and made not reversible by a
  patch that sets a state where none existed (1103). `needs_confirmation` is unchanged (1316),
  except that a plan whose input held a restricted or private item (`restricted = "include"`,
  B.2) always needs confirmation. Agents never confirm (1482-1488), and automations turn
  `NeedsConfirmation` into a refused run (runs.rs:583-591).
- **Confirmation bound to edits (D11).** `ApplyOptions` gains `expect: Option<String>` and loses
  its `Copy` derive. Every construction site is listed in step 10: the full literals at onto-intelli
  actions.rs:976 (`action.apply`, which passes the caller's `expect`), actions.rs:1411
  (`learned.certify` and `learned.reject`) and command.rs:146 (`ask`) gain `expect: None`; seed.rs:387
  and the test sites (section 2) compile unchanged or gain `expect: None`. The built-in confirm-true
  paths apply fixed built-in action types; if one is ever function-backed, it comes back unapplied
  with the message below, never confirmed blind.
  - **MCP `apply_action`** gains `expect` in its input schema (mcp.rs:321-325), and an unapplied
    result carries `edits_digest`. (Agents still never confirm; the field serves people's MCP
    clients.)
  - **`ask {confirm: true}`** cannot bind: its confirm re-runs the language model, which may choose
    another action or parameters than the preview showed. For a function-backed plan that needs
    confirmation it returns unapplied with "this action is computed by function escalate; apply it
    with action.apply and expect".
  - When the plan needs confirmation and `confirm` is set, `apply_action` requires
    `expect == plan.function.edits_digest`.
  - With `expect` missing, the receipt returns unapplied: "this plan was computed by function
    escalate; confirm with `expect` set to the `edits_digest` of the preview you saw".
  - With `expect` different, the receipt is `rejected` with `precondition_changed: the edits of
    function escalate changed since they were confirmed; preview again`.
  - An `inputs_changed` retry whose `edits_digest` still equals `expect` goes ahead: the inputs
    moved, but the edits did not.
  - The UI, the SDK helpers and the CLI (`onto do --confirm`) pass `expect` from the preview they
    showed.
- `drift()` stays as it is. For templated action types nothing changes: binding their confirmation
  is deferred (section 9).

### D.5 Receipts and undo

- Receipts live in KV, and `skip_serializing_if` keeps old receipts readable with no migration. The
  edit cap bounds the receipt.
- The `action.<name>` event payload gains `"function": {"name", "artifact", "edits_digest"}`, and a
  rejected one carries only host-written check text (D25).
- Undo is unchanged (1615). It applies the stored inverses and never runs the function, with the
  same "revision moved" refusal without `force`. A function cannot declare an inverse or claim
  reversibility.

### D.6 Automations

- **Closure.** It gains the `function:<name>` item (C.4).
- **Preparing outside the lock.** `evaluate_run` (runs.rs:682-685) and `commit_run` (700) each call
  `prepare_run(run, def, also)` before taking any guard. Under one read guard, `prepare_run` first
  evaluates the automation's `conditions` (as `evaluate_on` does at 565-569) and returns `NotMet`
  without running the guest when they fail, so a condition-filtered automation pays nothing for
  triggers it ignores. It then renders the action effect's parameters exactly as `evaluate_on` does
  (571-573), runs `pre_plan` (D.2), builds the input as the run principal, releases the guard and
  runs the guest. `evaluate_on` receives the result through the seed.
  - The output cache usually makes the second guest run, at commit, a hit.
  - The consumer's call site (onto-intelli automate.rs:459) and the dry-run call site (972) are
    unchanged.
- **Seed.** `PlanSeed::of(run.receipt_id(), run.started_at)` is stable across retries, so created
  ids are stable.
- **Outcomes.**
  - **Empty edits** give `NotMet`: no row and no receipt (B.4). A materialisation whose value did
    not change costs one guest run (usually a cache hit) and nothing else. If the edits were
    non-empty before the claim and empty at commit, the row is `failed` with
    `precondition_changed`, as for any `NotMet` at commit (runs.rs:749-751).
  - **Deterministic faults** are plan checks. Found at the pre-claim evaluation, they give a
    `refused` row (`plan_invalid: …`, onto-intelli automate.rs:459-472); found at commit, a `failed`
    row. Neither is retried. The row's reason, and so the `automation.run` event, holds only the
    host-written check text (D25).
  - **`functions_off`** (a switch, the operator setting, the denylist or a quarantine) is a failed
    plan check that the consumer (before the claim) and `commit_run` (at commit) handle apart from
    other invalid plans: the row is `skipped` with reason `functions_off: …`, so `automation.rerun`
    can run it after a resume (runs.rs:92-93), and
    the automation is paused once with `pause_automation` (onto-intelli automate.rs:234), recorded
    as `automation.paused`. A kill therefore never silently drops a materialisation's updates:
    the skipped rows say what was not run, and a person resumes and reruns them.
  - An output that deletes gives High risk, then `NeedsConfirmation`, then `refused`
    (runs.rs:583-591), and so does an input that read a restricted item. Lint predicts the first
    from `writes.delete`.
- **Retries.** `runs::retryable` (477-483) gains one rule: `Limited(m)` with `m` starting
  `functions:` is retryable, and `execute_run` (906) waits `max(retry.delay_ms(attempts),
  retry_after_secs × 1000)`, the bucket's true refill time. `inputs_changed` is already retryable:
  it is a `Conflict` without the `run_settled` or `precondition_changed` prefix. `function_capacity`,
  `function_internal` and `stopping` are `Unavailable`, which is already retried.
- **Budgets.** Automation runs are *background* callers. They draw fuel only while the tenant bucket
  is above `background_reserve_pct` (default 25%). Below it they get
  `Limited("functions: background reserve … retry in N s")` and wait, so a backfill cascade cannot
  starve people's calls. They also draw from the run principal's share (I.1). The consumer runs a
  tenant's runs one after another (onto-intelli automate.rs:147, 275), so automations hold at most
  one slot per tenant per process.
- **Shadow mode and dry runs** (`also`) build inputs with both principals' visibility.
- **Traces.** A run's trace is built from its row (284-304). The run's receipt carries the function
  block, so `run_trace` adds a `function.run` span from it.

---

## Part E. Computed properties

### E.1 Declaration

```toml
[functions.risk_index]
kind = "computed"
artifact = "sha256:…"
output = { type = "float", min = 0, max = 10 }
uses = ["now:day"]
names = { kinds = { ticket = "issue" }, rels = { blocked_by = "blocked_by" }, props = { score = "issue.risk_score" } }
input = { self = ["score"], walks.blockers = { from = "self", rel = "blocked_by", kinds = ["ticket"], depth = 3, limit = 200, props = ["score"] } }

[object_types.issue.computed]
risk_index = { function = "risk_index" }
```

Interfaces inherit computed properties the way `derived_defs` inherits derived ones
(schema.rs:1049). `derived` and `DerivedDef` are not changed.

### E.2 Evaluation on read (opt-in)

`Ontology::computed(p, ids, names, budget) -> ComputedOut {values, errors, stats}`:

```text
for each chunk of subjects (≤ 5,000 projected entities, and ≤ caps.scan items examined in all):
    self.read(|g, schema| for each visible subject and requested function:
        memo hit for (def_hash, subject, scope_key(p), revision) -> its cache key
        else build_input as p (scan-capped), key = cache_key(def, input), memoise)  // guard released after
for each (subject, function, key):                                                // no guard
    hit -> raw result | in flight -> join (single flight) | miss -> run, insert
    decode and check against `output` on every use (E.3)
    Ok(v) if output.accepts(v) -> values[id][name] = v
    Ok(_)  -> errors[id][name] = "function_output: value does not fit float"
    Err(e) -> errors[id][name] = e.code_and_message()
```

- A hidden subject is `not_found`, as for `get` (engine.rs:717-724).
- **The digest memo.** On an unchanged graph a hit would still rebuild every input just to find its
  key. A per-tenant memo maps `(def_hash, subject, scope_key(p), revision)` to the cache key, so a
  repeated read at the same revision skips the build. It is an LRU whose bytes count in the output
  cache's budget, and it is correct for the same reason the cache is: a new revision misses.
- The read guard is held per chunk, and each chunk's total scan is capped, so a page cannot hold
  the guard across an unbounded build while a writer waits.
- Values are opt-in. Nothing computes them unless the caller asks with `computed: true` or a list of
  names.
- Reads never fail because of a computed property. The entity and every other value come back, and
  each failure sits under its name in `computed_errors`.

### E.3 Caching: content addressed, single flight, bounded

- **Key**: `sha256("oi-fn/1" ‖ engine id ‖ artifact ‖ canonical(limits) ‖ input bytes)`. **Value**:
  what the sandbox itself produced, and nothing derived from the definition or the graph:
  `Arc<SandboxResult>`, which is the raw output bytes (≤ the output cap) plus the fuel, or a sandbox
  fault (fuel, memory, stack, trap, output over the cap or out of bounds, panic text from
  `oi_panic`). `Limited`, `Unavailable`, `Stopping` and `Internal` are never cached.
- **Checks after the cache.** Decoding, the envelope, aliases, `TypeDef`, ids that must be input
  entities and the plan's created-id check all run on every use. They are cheap and native, and they
  depend on the definition's `writes`, `names` and `output` (not in the key) or on graph state (the
  created-id check). So when an approver widens `writes` to fix a refusal, the next call with the
  same artefact and input succeeds at once, and a refusal never names a rule that no longer applies.
- **Correctness.** Everything the guest may depend on is in its input: which entities this reader
  may see (built through `visible`), the bucketed `now`, and, in the key, the code, limits and
  engine. Any write that matters changes the input, and a write that does not matter changes
  nothing. So there is no invalidation rule to get wrong. Readers share an entry only when their
  inputs are byte-identical, in which case each of them could have computed it.
- **Cost of a hit**: on an unchanged revision, a memo lookup (E.2); otherwise one scan-capped native
  input build and one SHA-256. The build's cost is set by what it examines, not by what it keeps
  (B.2), so "microseconds" holds only for small neighbourhoods; step 16 measures a hub of degree
  100k at depth 2.
- **Single flight.** A per-tenant map takes a key to `Arc<(Mutex<Option<SandboxResult>>, Condvar)>`.
  The leader's entry is an RAII guard (A.6): its `Drop`, which also runs when the leader panics,
  removes the entry and calls `notify_all`. Waiters use `wait_for` bounded by `leader_wait_ms`
  (default 5,000, checked at startup to be at least `queue_ms` plus twice the calibrated time of
  `caps.fuel`); past it they return `Unavailable("function_capacity: …")`. If the leader fails
  transiently, one waiter becomes the new leader. A popular `now:day` value rolling at midnight is
  computed once, not once per viewer.
- **Bounds.**
  - Per tenant: an LRU by bytes, `output_cache_mb` (default 16).
  - Process-wide: `output_cache_total_mb` (default 128), shared through one `AtomicU64` and a
    registry of each tenant's bytes. An insert over the total evicts, LRU first, from the tenant
    most over its fair share, `total / tenants holding entries`, which may be the inserting tenant.
    A tenant at or under its share is never evicted to make room for one over it, so tenants that
    became active later still cache, and how full other tenants' caches are cannot be read off
    one's own hit rate beyond the fair share.
  - Tenant cache bytes count in `Intelli::estimated_bytes` (intelli.rs:755-757), so tenant eviction
    sees them (hub.rs:473-517). Closing a tenant drops its cache.
- **Forks** share the parent's cache, which is safe because keys are content (F).

### E.4 Budgets per request

Each request that asks for computed values carries a budget:

- `request_fuel` (default 500M): tenant fuel spent on this request's misses, instantiation and
  input scans included;
- `max_computed_per_request` (default 2,000): subject × function evaluations, hits included.

Computed reads are reads (I.1): they stop above `write_reserve_pct` and draw from the reader's share.

Past either one, the remaining values get `computed_errors: "function_budget: request budget
spent"`. The entity data is still returned.

### E.5 Surfaces and fan-out guards

- `entity.get {computed: true | [names]}` adds `computed` and `computed_errors` beside `derived`.
  The new read `entity.computed {id, names?}` returns just those. Both are in `GRAPH_READS`.
- **MCP `get_entity`** gains `computed` (default false). The forced `derived = true` (mcp.rs:948-950)
  stays and never triggers user code. Its description (mcp.rs:98) changes from "with computed
  properties" to "with derived properties (and computed ones when asked)".
- **`EntityCard`** keeps asking for `derived` (EntityCard.tsx:38), and its existing section titled
  "Computed" (101), which shows derived aggregates, is renamed "Derived". It asks
  `entity.computed` separately for the one entity it shows, and renders those under a new
  "Computed" section, so a slow or failing computed value never delays the card and the two
  sources never share a name. The doc comment on `ObjectType.derived` (schema.rs:218) says
  "Derived properties".
- **Current only.** Computed values are evaluated on the live graph with the head definition and
  the current `now`. `entity.as_of`, `entity.history` and `map.snapshot` refuse a `computed`
  argument with `invalid_argument: computed values are evaluated on the live graph only`, rather
  than ignore it, since their inputs are otherwise parsed leniently.
- **`objects.explore {computed: [names]}`** (`explore`, explorer.rs:559) evaluates only for the
  returned page (≤ `MAX_LIMIT` = 1,000, explorer.rs:28), within the request budget. It is shown as
  a new `Field` variant (explorer.rs:206). **Sort and filter on computed values are refused** with
  a pointer to materialisation (E.7): "sorting by computed risk_index is not supported; materialise
  it (docs/FUNCTIONS.md)".
- `find`, search, `ask`, `context` and `answer` do not see computed values in phase 2, so there is
  no fan-out into LLM context.
- Nor do the change feed, webhooks, automation triggers, the reasoner (`NOT_EVALUATED`,
  reason.rs:81) or certificates.

### E.6 Switches and failures

When functions are off (by the process, the operator or the tenant kill), the function is off, or
its digest is denylisted or quarantined, each requested value is listed in `computed_errors` with
`functions_off: …`. The read succeeds.

### E.7 Materialisation pattern

When a value must be sorted, filtered or subscribed to over many entities, write it:

- An automation on changes to the input's kinds (for example `issue` updates and `blocked_by`
  links) applies a function-backed action whose envelope patches one stored property
  (`risk_stored`).
- Fan-in ("a blocker changed, so recompute the blocked issues") is a walk over the inverse relation
  plus several patches.
- **Cost**: one run, one receipt and one change record per recompute that changes the value, within
  phase 1's automation rates and subject budgets. A recompute whose value did not change returns
  `edits: []`, which is `NotMet`: one guest run (usually a cache hit), no row and no receipt (D.6).
  A restricted subject needs `restricted = "include"`, and its runs then need a person, so a
  materialisation covers only items that are neither restricted nor private.
- **Loop guard**: the automation does not trigger itself unless `allow_self` is set
  (automate.rs:353-357); `automation_loop` (lint.rs:1902) reports cycles through other automations
  from the function's `writes` envelope (C.8), and phase 1's depth limits apply.
- **During a kill**, runs are `skipped` and the automation is paused (D.6); after a resume, a person
  re-enables it and reruns the skipped rows, so no update is dropped silently.

docs/FUNCTIONS.md shows this pattern and its cost line by line. A native materialised index is
deferred (section 9).

---

## Part F. Query functions

```toml
[functions.top_risks]
kind = "query"
artifact = "sha256:…"
params = { team = { type = "string", required = true } }
output = { type = "list", max = 200, items = { type = "object", fields = { issue = { type = "entity", kinds = ["ticket"] }, score = { type = "float", optional = true } } } }
# names, input.finds / input.walks as in B.2
```

- `function.call {name, params, workspace?, expect_signature?}`:
  1. `admit` (any role with read);
  2. parameters are normalised with `ParamDef`, entity parameters visibility-checked as in action
     planning;
  3. the input is built as the caller under one read guard, with finds rendered strictly and
     scan-capped (a `find` with `eq` examines its kind);
  4. `run_cached`, as a read: it draws on the caller's share and stops above `write_reserve_pct`
     (I.1), and each miss pre-charges its declared fuel;
  5. the `value` is checked against `output` (`entity` values must be input ids).
- **Response**: `{ "value", "function", "artifact", "signature", "revision", "input_digest", "fuel",
  "cached", "more": ["issues"] }`. `more` names the finds and walks cut at their limit, so a caller
  never takes a page for the whole.
- **Read-only.** There are no writes, no `actor` unless declared, and no audit event: reads are
  traced (Part I), not audited.
- **Forks.** `function.call` is in `GRAPH_READS` (actions.rs:1991), so `scenario.run` and MCP
  `what_if` run it on the live graph and on a fork. `fork()` shares the parent's function host
  (engine, module cache, semaphore), the tenant handle (bucket, switches, output cache) and the
  artefact source through `Arc`s. The fork's `MemoryStore` has no artefacts, and a fork never
  writes any.
- Routing natural language to query functions is deferred.

---

## Part G. Author toolchain

### G.1 Guest crate `onto-fn` (`sdk/rust-fn`)

- **Membership.** It is a workspace member. Its dependencies are `serde` and `serde_json` only, so
  there are no new crates.
- **What it provides:**
  - `oi_alloc`;
  - an `onto_fn::entry!(escalate => handler, risk_index => handler2)` macro that writes `oi_run`
    and dispatches on `fn`;
  - typed `Input`, `Walk<T>` (with `more`) and `Edit` builders;
  - `Output::{edits, value, error}` and `message`.
- **Building.** `cargo build --release --target wasm32-unknown-unknown`, with the template's
  `.cargo/config.toml` setting `-C link-arg=-zstack-size=65536` so the initial memory stays far
  below `caps.initial_memory_kib` (A.3).
- **Panics.** wasm32-unknown-unknown aborts on panic (`panic-strategy: abort`), so the guest never
  returns from `oi_run`: the panic hook runs, then `unreachable` traps. The `onto-fn` hook writes the
  message (≤ 256 characters) to a static buffer and the crate exports `oi_panic() -> i64`
  (ptr << 32 | len), which the host reads after the trap (G.5).
- **The example**, `examples/functions/escalate`, is a workspace member with
  `crate-type = ["cdylib", "rlib"]`, so its handler logic is unit-tested on the host.
- The guest crate needs `unsafe` for the raw-pointer ABI (`#[unsafe(no_mangle)]`). It is guest
  code and outside the TCB.

### G.2 Typed bindings (`function.types`)

`function.types {name | def, lang, for}` renders text from the definition and the target schema. The
engine renders it, and the CLI only writes the file, so local and remote generation give identical
files.

- `lang = "rust", for = "guest"`:
  - one struct per kind alias, with `id`, `kind`, `title`, `state` and a `props` struct whose
    declared aliases are `Option<T>`, typed from `PropType`;
  - one struct per function input (`params` or `self`, plus `data` walks as `Walk<Item>`);
  - constants for every alias in `names` and every `writes` entry, so `Edit::patch(id).set(SCORE, 5)`
    misspelt does not compile;
  - the output type.
- `lang = "ts", for = "client"`: types for query function signatures (parameters to result, from
  `TypeDef`) and the computed names per kind, so `client.functions.call("top_risks", { team })` is
  typed in product repos.
- The header names the schema version and fingerprint. `onto fn build` warns "types are stale: run
  `onto fn types`" when they are older than the target schema. TS guest bindings come with phase 2.1.

### G.3 Project layout and `onto fn build`

```text
delivery/
  onto-functions.toml      # [project] name; [build] command and module path; [target] remote or schema dir
  Cargo.toml               # depends on onto-fn
  functions/escalate.toml  # [functions.escalate] … with artifact = "@build"; may also hold the action type
  src/lib.rs               # onto_fn::entry!(…)
  src/generated.rs         # `onto fn types` output, committed, "do not edit"
  cases/escalate/*.toml    # test cases (G.4)
```

`onto fn build`:

1. runs `[build] command` (default `cargo build --release --target wasm32-unknown-unknown`):
   - with every `ONTO_*` variable and the onto token removed from its environment, since a project
     may be written by an agent and the command runs on a person's machine;
   - a command other than the default is printed and runs only with `--trust-build`;
2. loads the module and checks the module rules with the embedded `onto-sandbox` (onto-cli depends
   on onto-intelli, onto-cli Cargo.toml:17);
3. validates the definitions against the target schema with the same onto-core code as
   `check_functions`;
4. prints the digest and a summary (kind, input, envelope, limits, initial memory).

### G.4 Tests: `function.test` and `onto fn test`

Cases are TOML files under `cases/<function>/`. Every case runs in the real sandbox.

```toml
# cases/risk_index/blocked.toml
subject = "issue:1"
[graph]                                   # a fixture graph, loaded into an in-memory ontology
entities = [ { id = "issue:1", kind = "issue", props = { risk_score = 3 } },
             { id = "issue:2", kind = "issue", state = "open", props = { risk_score = 5 } } ]
links    = [ { from = "issue:1", rel = "blocked_by", to = "issue:2" } ]
[ctx]
now = "2026-10-29T10:00:00Z"
as = "user:ann"                            # optional principal for visibility cases; graph fixtures only
[expect]
value = 8.0                                # or edits = [...], preview = [...], error = { class, message }
max_fuel_percent = 50
```

- **Sources of input.** A case gives exactly one of:
  - `graph`: a fixture;
  - `input`: the input document itself, with no graph;
  - nothing: with `--remote`, the subject or arguments are resolved against the live tenant as the
    author, read-only.
- **`ctx.as` is valid only with a `graph` fixture.** Against the live tenant (a subject, arguments
  or `--remote`) the input is always built as the authenticated caller, and a case that sets `as`
  is refused, so a test is never an act-as path.
- **An explicit `input` never touches the live tenant.** For actions, it is planned against the
  case's fixture graph, or an empty graph when there is none; ids that do not exist there plan as
  the fixture says. So a caller-written input cannot make a hidden id "appear" in the live graph, and
  plan warnings never reveal whether links to hidden entities exist.
- **`expect`** is one of `value`, `edits`, `preview` (the plan's preview lines), or
  `error = {class, message}`.
- **Checks beyond `expect`:**
  - **Determinism**: each case runs twice, and the outputs and fuel must be identical. With zero
    imports a guest cannot read an undeclared source, so this guards the engine and the input
    builder.
  - **Headroom**: fuel and memory are reported against the limits. `max_fuel_percent` fails a case,
    and use above 50% warns.
  - **Cuts**: walks reported with `more = true`.
- **`--record`** writes the resolved `input` of `--remote` cases into their files, turning live
  checks into offline golden tests.

The server exposes the same runner as `function.test` (onto-core `functions/test.rs`, shared with
the CLI). It accepts a published `name`, or a draft `{artifact, def}` (subject to `functions.drafts`,
C.7), and a fixture, subject, arguments or explicit input. It never commits. For actions it returns
the full plan (preview, risk, confirmation, inverse, `edits_digest`) that `action.plan` would,
against the live tenant for a subject or arguments, and against the fixture or an empty graph for an
explicit input. Every response looks like this:

```json
{ "function": "risk_index", "artifact": "sha256:3c9e…", "engine": "wasmi-2.0.0/7a1c…",
  "subject": "issue:1", "input_digest": "sha256:1f0a…", "input": { "…": "…" },
  "value": 8.0, "log": ["blockers=1"],
  "stats": { "fuel": 1204331, "fuel_limit": 10000000, "memory_peak_kib": 1120, "memory_limit_kib": 16384,
             "ms": 1.4, "input_entities": 2, "input_bytes": 412, "cache": "miss", "more": [] },
  "checks": { "deterministic": true }, "warnings": [], "trace_id": "…" }
```

`input` and `log` are returned to the caller, who could see that data anyway. In traces they are
stored only with `capture_content`.

### G.5 Debugging: `function.input` and errors authors see

- `function.input {name, subject | params}` returns the assembled input for a subject as the caller,
  read-only, with its digest and `more`. It is how an author sees exactly what the code will read.
- Every failure the caller sees carries its class (B.7), the function, the subject or arguments,
  the input digest, the stats and, in `detail`, the guest's own text, for example:

  ```text
  function_error: escalate for issue:42: panicked: score out of range · input sha256:77b2… (issue, 14 blocked) · fuel 2.1M of 50M · memory 2.3 MiB of 16 MiB
  ```

  That full line goes to the caller (`function.test`, `function.call`, an action's response and
  receipt) and to traces with `capture_content`. The plan-check text that reaches events and run
  rows is the host's short form, `function_error: escalate returned an error (input sha256:77b2…,
  fuel 2.1M)`, without counts, subjects or guest text (D25).
- **Rust panics.** wasm32-unknown-unknown aborts on panic, so a panic is an `unreachable` trap and
  `oi_run` never returns. When the module exports `oi_panic` (G.1), onto-sandbox calls it once after
  an `Unreachable` trap, under a fixed budget of 100k fuel counted in the call's fuel, checks that the
  returned range lies in memory, and reports `function_error` with `detail = "panicked: <message ≤
  256>"`. A module without the export, or an `oi_panic` that fails, gives a plain `function_trap`.

### G.6 `onto fn diff` and `onto fn publish`

- `onto fn diff` uploads nothing. It prints the change set publish would file, with the server's
  `schema.plan`: risk, lint, function previews (C.5) and signature changes.
- `onto fn publish`:
  1. uploads the module (`function.upload`, idempotent) and checks that the digest the server
     returns equals the SHA-256 of the module file it built, refusing otherwise;
  2. replaces `artifact = "@build"` with the digest;
  3. builds the change set (`put_function` per definition, plus `put_action_type` and object-type
     computed attachments when the files hold them; with `--prune`, `delete_function` for this
     project's functions that are gone);
  4. files it with `schema.propose` (the default, and always for agents), or with `schema.apply`
     under `--apply` when the caller `can_apply`. The proposal's note records the build command
     used, marked unverified, so reviewers see how the bytes were made.

  It never enables an automation.

---

## Part H. Surfaces

New module `functions::CALLS` in onto-intelli (`crates/onto-intelli/src/functions.rs`), dispatched
like `automate::CALLS` (actions.rs:948-950). Every action called with `{}` fails with
`invalid_argument` or is a harmless read.

| Action | Write | gRPC (`FunctionService`) | HTTP | MCP | CLI |
|---|---|---|---|---|---|
| `function.list` (sorted by name: kind, digest, signature, limits, used by, off state) | | List | `GET /v1/functions` | `list_functions` | `onto fn list` |
| `function.get` | | Get | `GET /v1/functions/{name}` | | `onto fn get` |
| `function.call` (query; `GRAPH_READS`) | | Call | `POST /v1/functions/{name}/call` | `call_function` | `onto fn call <name> k=v…` |
| `function.test` | | Test | `POST /v1/functions/test` | `test_function` | `onto fn test [--remote] [--record]` |
| `function.input` | | Input | `POST /v1/functions/{name}/input` | | `onto fn input` |
| `function.types` | | Types | `GET /v1/functions/types?lang=…&for=…` | | `onto fn types` |
| `function.upload` | w (agents yes) | Upload (`bytes module`) | `PUT /v1/functions/artifacts` (raw, own limit) | `propose_function` (authoring; upload plus forced proposal, like `propose_ontology`) | `onto fn upload`, `onto fn publish` |
| `function.artifact` (info) | | Artifact | `GET /v1/functions/artifacts/{digest}` | | |
| `function.stats` | | Stats | `GET /v1/functions/stats` | | `onto fn stats` |
| `function.disable` / `enable` | w (admins; not in `AGENT_WRITES`) | Disable / Enable | `POST /v1/functions/{name}/disable\|enable` | | `onto fn disable\|enable` |
| `function.kill` / `resume` | w (admins; not in `AGENT_WRITES`) | Kill / Resume | `POST /v1/functions/kill\|resume` | | `onto fn kill\|resume` |
| `entity.computed` (`GRAPH_READS`) | | via `Call` | `POST /v1/call/entity.computed` | `get_entity {computed}` | `onto show --computed` |
| `functions.purge` (operator; hub admin credentials, not a tenant action) | w | | `POST /v1/admin/tenants/{tenant}/functions/purge` | | `onto admin functions purge` |

- **Schema edits** stay `schema.plan`, `propose` and `apply` with `put_function` and
  `delete_function`. `schema.proposal` (get) accepts `preview_functions` (C.5).
- **`action.plan` and `action.apply`** accept `expect` (D11), and plans carry `function`. MCP
  `apply_action` gains `expect`; `ask {confirm}` returns a function-backed plan that needs
  confirmation unapplied, with a pointer to `action.apply` (D.4).
- **As-of reads.** `entity.as_of`, `entity.history` and `map.snapshot` refuse `computed` (E.5).
- **MCP.** `list_functions` and `call_function` go in the retrieval, catalog and reasoning profiles;
  `test_function` and `propose_function` in the authoring profile. `what_if` covers `function.call`
  through `GRAPH_READS`. One generic tool keeps the static table (mcp.rs:71) and
  `listChanged: false`.
- **gRPC.** `Struct` responses in the phase-1 style, routed through `BUILTIN_ROUTES`
  (routes.rs:21). Users who want a typed RPC per query function map one in their own proto to
  `function.call` with a request template; routes validate only that the action exists
  (routes.rs:251-257).
- **HTTP.** The routes are listed in the module doc (http.rs:1-84).
- **CLI.** `onto fn …` lives in a new `crates/onto-cli/src/functions.rs`, reusing `parse_params`
  (main.rs:900). `onto do --confirm` passes `expect` from the plan it printed.
- **SDKs.** TypeScript and Python get a `functions` namespace (`list`, `get`, `call<T>`, `test`,
  `input`, `upload(bytes)`, `types`), `entities.get(id, {computed})`, and `actions.apply(…, {confirm,
  expect})`. Both stay zero-dependency.
- **UI** (`ui-dist` rebuilt in the same commit, ci.yml:187-200):
  - a **Functions** nav entry (Shell.tsx:11-20 `NAV`, main.tsx:154 `ONTOLOGY`): the list; a detail
    view (definition, names, input, writes, limits, digest, signature, used by, stats, off state,
    with admin disable and kill buttons); and a **test console** that reuses `ParamInput` and
    `EntityPicker` (ActionDialog.tsx:208, 242) and shows input, output or plan preview, fuel and
    memory bars, `more` and the trace link;
  - the Proposals page shows the function diff and previews;
  - `ActionDialog` shows "computed by function X", then the function's message after the preview
    lines in a distinct, labelled style (D.4), and sends `expect` with `confirm`;
  - `EntityCard`'s existing derived section is renamed "Derived", and a new Computed section
    renders computed values and `computed_errors` reasons (E.5);
  - Explorer gets computed columns for the page only, with sort disabled and the reason shown.

---

## Part I. Limits, quotas, kill switches, traces, audit

### I.1 Three layers of limits

| Layer | Where | Values | Enforced |
|---|---|---|---|
| Per function | schema `limits` (defaults from `[functions.defaults]`) | `fuel`, `memory_mb`, `input_kib`, `output_kib`, `scan`, `writes.max`, walk and find limits | at plan, propose and apply (≤ caps) and at run |
| Per process caps | `[functions.caps]` | upper bounds for each of the above | config `check()`; plans refuse above the caps; a stored definition above a lowered cap opens and its calls refuse (B.7) |
| Per tenant | project settings JSON `functions.{enabled, drafts, fuel_per_minute, max_concurrent, max_concurrent_per_principal, principal_share_pct, agent_share_pct, request_fuel, max_computed_per_request, background_reserve_pct, write_reserve_pct}` through `limits_of` (onto-control llm.rs:135-149), else `[functions.tenant]` | the operator switch, drafts, the fuel bucket (burst = 1 minute), principal shares, concurrency, request budgets, reserves | per process, at admission |

- **Fuel bucket.** Each call pre-charges its declared fuel plus its instantiation charge and is
  refunded the unused part afterwards, through an RAII guard (A.6), so concurrent calls cannot
  overshoot and a panic cannot leak a charge. Cache misses pay for the guest; every input build pays
  for its scan (`scan_fuel_per_edge`, B.2), hits included, unless the digest memo (E.2) skipped it.
- **Principal shares.** Each principal may draw at most `principal_share_pct` (default 50) of
  `fuel_per_minute` per minute, and agents `agent_share_pct` (default 20), each through the same
  bucket arithmetic, so one reader, agent or reloaded proposal cannot drain the tenant. Previews are
  charged to the viewer who asked (C.5).
- **Reserves.** Three classes draw on one bucket. Background callers (automation runs) stop when
  the bucket is below `background_reserve_pct` (default 25%). Reads (`function.call`, computed
  values, `function.test`, `function.input` and previews) stop below `write_reserve_pct` (default
  10%). Function-backed actions applied by people may drain it. Each stop is
  `Limited("functions: … retry in N s")` with the true refill time.
- **Zero means zero.** For function limits in project settings, `0` is a real value (no fuel, no
  concurrency), not "unset" as `limits_of` treats LLM limits (142); an absent key takes the
  `[functions.tenant]` default.
- **Wording.** "Function budgets and concurrency are per process."

### I.2 Kill switches

- **Process.** `[functions] enabled = false` is the default. Every call refuses with
  `functions_off: process`. Changing it needs a restart, as for `[automation] enabled`.
- **Operator, per tenant.** The project setting `functions.enabled = false`, written only through
  the control plane, turns functions off for one tenant on every replica without a restart. Tenant
  admins cannot override it: `function.resume` refuses with `functions_off: operator`. It arrives
  with the tenant's other project settings (`TenantSettings` gains `functions`, hub.rs:50-63).
- **Tenant.** KV `function.control/kill {on, by, at, reason}`, written through `commit_config` by
  admins, never agents, and audited.
- **Per function.** KV `function.state/<name> {disabled, by, at, reason}`, with the same rules.
- **Digest denylist.** `[functions] denylist_file` names a file of `sha256:` digests, one per line,
  owned by the operator (a Helm ConfigMap reaches every replica). The process re-reads it when its
  modification time changes, checked at most once per `control_poll_ms`. A denylisted digest is
  refused at upload, at compile, at call start and at every slice boundary (`functions_off:
  denylisted`), in every tenant.
- **Quarantine.** A digest whose runs end in `Fault::Internal` (an interpreter panic, A.4)
  `quarantine_after` times (default 3) within an hour is refused on that process (`functions_off:
  quarantined`) until a restart. It is logged with the digest, and the operator decides whether to
  add it to the denylist file.
- **Config namespaces.** `function.control` and `function.state` are added to `CONFIG_NAMESPACES`
  (outbox.rs:39-40).
- **Propagation.** KV-only commits fire no listeners (engine.rs:1549-1561), so each process keeps
  the flags in memory. It refreshes them lazily at most once per `control_poll_ms` (default 1,000),
  at call start and at slice boundaries, and at once on the replica that wrote them. In-flight
  guests stop at their next slice (`off`).
- **`system.health`** shows the process switch, the operator setting, the kill, the number of
  functions off, the denylist size and its file's age, and quarantined digests (section 5).
- **There is no breaker for function faults.** Deterministic faults are per call, and automations
  already pause after repeated failures. Interpreter panics are the exception, and quarantine
  handles them.

### I.3 Traces

- **Call spans.**
  - `function.call`, `function.test` and `function.input` are selected by `traces_call`
    (actions.rs:849-855) like `LLM_ACTIONS`, because they spend CPU. `function.upload` is traced
    without content.
  - Action applies are traced already, as writes. Their root span gains `function`,
    `function_artifact` (12 hex), `engine`, `fuel`, `cached`, `input_digest` and `edits_digest`.
- **Child spans** of kind `tool`: `function.build_input` (entities, bytes, µs) and `function.run`
  (digest, fuel, µs, slices, queue µs, outcome class).
- **Computed reads** add one aggregated `function.computed` span per request (functions, subjects,
  hits, misses, failures by class, fuel), when the request is traced.
- **Content.** Input and output documents, `message`, `error`, `log` and panic text are recorded
  only with `capture_content`, clipped to `max_content_bytes`, and redacted for observers who are
  not admins. Without it, spans carry the class, digests, counts (`scanned` included) and fuel only.
- **Where the tracer comes from.** In dispatch, onto-intelli reads the task-local tracer
  (onto-intelli trace.rs:196-207), which `block_in_place` keeps; onto-core returns the stats with
  each result. Automation runs get their span from the run row (D.6).

### I.4 Audit

| What | Record |
|---|---|
| Artefact upload | `function.artifact` event in the same commit as the row. |
| Operator purge | `function.artifact.purged` event in the same transaction as the delete (C.1). |
| Disable, enable, kill, resume | events through `commit_config`; refusals through `refuse_configure`. |
| Definition changes | `schema.change` events and proposals, as today. |
| Writes | receipts with the `function` block, and the `action.<name>` event with `function`. |
| `function.call`, `function.test`, `function.input`, computed reads | no audit events, like every other read; visible in traces and stats. |

## Part J. Configuration (`crates/onto-intelli/templates/onto-intelli.toml`, onto-intelli config.rs `FunctionsConfig`)

```toml
[functions]
enabled = false                    # process kill switch (D22)
max_concurrent = 4                 # process semaphore, handed out round-robin by tenant
max_concurrent_per_tenant = 2
queue_ms = 200                     # wait for a slot, then Unavailable("function_capacity: …")
leader_wait_ms = 5000              # a single-flight waiter's bound (E.3)
slice_fuel = 10_000_000            # switches, denylist and shutdown are checked this often
control_poll_ms = 1000             # tenant kill, per-function off, operator setting, denylist file
denylist_file = ""                 # sha256 digests refused in every tenant (I.2)
quarantine_after = 3               # interpreter panics per digest per hour before quarantine
max_functions = 100                # per tenant; checked at apply and propose
artifact_quota_mb = 256            # per tenant
agent_artifact_quota_mb = 32       # per agent principal
uploads_per_minute = 10            # per principal
module_cache_mb = 64               # per process
output_cache_mb = 16               # per tenant, counted in estimated_bytes
output_cache_total_mb = 128        # per process, fair-share eviction
preview_fuel = 200_000_000         # per schema plan or requested proposal preview
scan_fuel_per_edge = 50            # input building, charged to the bucket (B.2)
max_stack_kib = 1024               # engine-wide

[functions.defaults]               # a definition's limits default to these
fuel = 50_000_000                  # action and query; reset from step 1's worst-mix calibration
fuel_computed = 10_000_000
memory_mb = 16
input_kib = 256
output_kib = 64
edits = 100
scan = 20_000

[functions.caps]                   # a definition may not ask for more
module_kib = 2048
initial_memory_kib = 2048          # a module rule (A.3)
memory_mb = 64
fuel = 1_000_000_000               # reset so that fuel × worst ns per fuel ≤ 5 s (A.5)
fuel_computed = 100_000_000
input_kib = 1024
output_kib = 256
edits = 1000
entities = 10000
scan = 100_000
depth = 3

[functions.tenant]                 # defaults; project settings override them per project (0 = zero)
fuel_per_minute = 10_000_000_000   # per tenant, per process
principal_share_pct = 50           # of fuel_per_minute, per principal per minute
agent_share_pct = 20               # the same, for agents
max_concurrent_per_principal = 1
request_fuel = 500_000_000
max_computed_per_request = 2000
background_reserve_pct = 25        # automation runs stop below this
write_reserve_pct = 10             # reads stop below this; function-backed actions may drain it
drafts = "people"                  # who may run drafts and request previews: people | writers | off
```

- `FunctionsConfig::check()` follows `AutomationConfig::check` (config.rs:1020-1040). It refuses
  zero or inverted values and defaults above their caps, requires `write_reserve_pct <
  background_reserve_pct` and `leader_wait_ms > queue_ms`, and reports the function memory sum at
  startup with the cgroup warning (A.5). The deploy/helm values gain a sizing comment next to the
  memory limit.
- The engine receives `OntologyOptions.functions` (engine.rs:32-71): `enabled`, the caps, the
  defaults and `max_functions`.
- The Hub creates one `FnHost` per process (engine, module cache, round-robin semaphore, stop
  flags, denylist, quarantine, the output-cache byte registry) and hands it to every tenant's
  `Ontology`. Each `Ontology` holds its own `FnTenant`: bucket with principal shares, switches
  (operator setting included), verified digests, output cache, digest memo, preview cache and
  stats.

---

## 4. Multi-tenant and multi-replica behaviour

| Thing | Scope | Across replicas |
|---|---|---|
| Artefact bytes | tenant table (PostgreSQL) | shared, immutable |
| Definitions | tenant schema | shared; a lagging replica uses the older definition until it catches up, never a mix |
| Compiled modules | process, by digest, all tenants, each tenant gated by its verified digests | per process; content-addressed, no data |
| Denylist, quarantine | process (file, panics) | the denylist file reaches every replica; quarantine is per replica |
| Output cache | process × tenant, by content | per process; each correct on its own; no coherence protocol is needed |
| Slots, buckets, request budgets | process × tenant | **approximate** (per process), as the docs say |
| Kill and off flags | tenant KV | within `control_poll_ms` on every replica |
| Stats | process | per replica; admin views say which replica |
| Automation consumer | lease holder (phase 1) | at most one guest per tenant at a time from automations |
| Writer fence | `head.min_writer` | phase-1 replicas cannot read most tenants with functions, and cannot write any (C.9) |

- **Isolation.**
  - Artefacts, output caches, buckets and switches are per tenant. Only compiled modules are
    shared, and a tenant reaches one only for a digest whose row it holds and has verified (A.7).
  - Each call gets a fresh store and instance, so a guest never sees another subject's input, let
    alone another tenant's.
  - All tenants share the interpreter's process. A hub whose tenants do not trust each other keeps
    functions off until the out-of-process option exists (A.8, section 9).
- **Fairness.** Between tenants: the process semaphore handed out round-robin by tenant with a
  bounded wait, the per-tenant concurrency cap, and the output-cache total with fair-share eviction.
  Within a tenant: per-principal fuel shares (smaller for agents) and concurrency, the background
  and write reserves, and request budgets.
- **Memory.**
  - Output caches count in `estimated_bytes`, under the process total.
  - The module cache has one process cap.
  - Guest memory is bounded by `max_concurrent × caps.memory_mb`, and each instance's initial memory
    by `caps.initial_memory_kib`.
  - Their sum is logged at startup and checked against the cgroup limit (A.5).
- **Eviction.** A request in flight keeps its `Arc<Intelli>` and finishes within its fuel, while
  the cache goes with the slot.
- **Deletion.** Deleting a project stops new calls through the existing tenant refusal
  (onto-control hooks.rs:176-180).
- **Rolling deploys.** 0015 is additive. The first function must come after the rollout: from then
  on a phase-1 replica cannot catch up on that tenant, and the fence (C.9) covers what remains.

## 5. Observability

- **Stats** live per process × tenant × function in a bounded map: the top 100 functions per tenant
  by fuel, with the rest summed as `other`. Each entry holds:
  - calls, subjects, cache hits, misses and negative hits;
  - fuel (sum, p50, p99) and µs (p50, p99), in fixed-bucket histograms with no new dependency;
  - input scan counts (p50, p99, and builds refused by the scan cap);
  - queue wait p99;
  - failures by class;
  - cut walks (`more`).

  The windows are 1 minute and 1 hour, rolling. `function.stats` returns them for this replica.
- **`system.stats` and `GET /v1/admin/tenants`** (`TenantStat`, hub.rs:138) gain, per tenant: output
  cache bytes and entries, fuel used per minute, calls in flight, the top 5 functions by fuel over
  1 h, and failures by class over 1 h. Per process: slots busy and waiting, refusals by class,
  module cache bytes, entries and compiles (count, p99 ms), the measured fuel per ms, and the
  functions' resident memory (module cache, output caches, guest memory in use) against the startup
  sum (A.5).
- **`system.health`** (actions.rs:1449) adds `functions: {enabled, operator_off, engine, kill,
  off: n, denylist: {entries, file_age_s}, quarantined: [digest prefixes], fuel_per_ms}`.
- **`tracing` spans** `function.run` and `function.compile` carry the digest prefix, tenant, class
  and µs, never guest text.
- Counters read only in-memory state or bounded KV ranges, never a table scan (the phase-1 counters
  rule).

## 6. Test plan

**Unit tests in `onto-sandbox`** (WAT fixtures, many generated per test with `format!` and
`wat::parse_str`). They run in plain `cargo test` (debug, with the A.1 override) on Linux, macOS and
Windows:

- each module rule: an import, a start function, a missing or mistyped export, two memories, shared
  memory, an oversize module, an oversize table, initial memory over `caps.initial_memory_kib`, and
  `oi_panic` with the wrong type;
- **dispatch in debug**: a loop of at least 200M fuel and guest recursion to the configured
  maximum depth, each on a 2 MiB thread (the test-thread default), complete without a native stack
  overflow, so a feature-set regression fails CI instead of crashing it;
- out of fuel at the same instruction twice;
- **sliced equals unsliced**: for `slice_fuel` of 1,000, 10M and unlimited, identical output, fuel
  and outcome digest for a loop, a recursion, a 16 MiB `memory.fill` (with the 1,000 slice the test
  asserts `required_fuel` exceeded a slice at least once), and a `memory.grow` and a `table.grow`
  whose cost straddles a slice boundary;
- the limiter: a grow over the memory limit is `Fault::Memory`; a grow that runs out of fuel is
  `Fault::OutOfFuel`, never a memory trap;
- instantiation is charged: two modules that differ only in initial memory differ in fuel by
  `bytes / 64`, and a module whose instantiation charge exceeds `limits.fuel` is out of fuel;
- the stop flags stop at the next slice (`Off`, `Stopping`), and a denylisted digest stops there too;
- a panic inside a run (a test-only fault-injection hook) is `Fault::Internal`, not cached, and
  counted toward quarantine;
- `oi_panic`: an `unreachable` trap with the export gives `Panicked(text)` within its 100k fuel; a
  failing or out-of-bounds `oi_panic` gives a plain trap;
- stack overflow;
- an out-of-bounds output pointer, and output over the cap;
- fuel identical on a cold and a warm compile (Eager);
- bulk `memory.fill` charged by size;
- NaN canonicalisation;
- the cross-OS vector, with exact output digest and fuel;
- **calibration** (also run in release by `onto-bench`): ns per fuel of the simple loop and of the
  adversarial mix (A.5); the worst mix within 8× of the simple loop.

**onto-core unit tests (in-file):**

- `functions/def.rs`:
  - compile checks, alias resolution, roots per kind, the entity cap sum, `deny_unknown_fields` and
    round-trip;
  - `writes` naming `acl`, `visibility` or `member_of` is refused; `restricted` on a non-action
    function is refused;
  - a schema without functions keeps its fingerprint, an action type without `function` keeps its
    `hash_of`, and an object type without `computed` keeps its bytes;
  - `TypeDef` validation paths;
  - the signature is stable across field order.
- `functions/input.rs`:
  - hidden entities are absent (workspace, `restricted` with `acl`, a `member_of` team) and present
    for privileged readers;
  - with `restricted = "exclude"`, a restricted walk item is absent for an admin caller and never
    sets `more`, and a restricted entity parameter refuses; with `include` it is present and the
    run records `restricted_read`;
  - `more` counts visible items only, and `on_limit = "error"` refuses;
  - the scan cap: a hub node whose degree exceeds `limits.scan` refuses with the fixed text, the
    count includes hidden edges, and the refusal text is identical whether the extra edges are
    hidden or visible; the count is equal on two graphs built in different orders;
  - the `also` intersection;
  - order and limit are stable under insertion-order permutations (two graphs built in different
    orders give identical bytes);
  - canonical bytes are sorted when keys are inserted out of order (`canonical_json`);
  - strict `$param` rendering refuses a missing value;
  - input over the cap refuses;
  - `now` buckets.
- `functions/output.rs`:
  - each B.4 rule (an id that is not an input entity, including one taken from `actor`, `parent`,
    a property or a text parameter; `unlink.to` outside the input; an edit naming a created id; a
    duplicate patch; an alias outside `writes`; `replace` and an event payload refused as unknown
    keys; the event namespace; unknown keys), with all violations listed and sorted;
  - violation text never contains a guest-produced string (an invented id appears only in `detail`);
  - `message` with bidi overrides, zero-width characters and decomposed forms comes out stripped and
    NFC-normalised;
  - empty `edits` decode to an empty edit set;
  - `edits_digest` independent of the seed;
  - a property test feeding random JSON to the decoder (no panic: a refusal or valid edits).
- `functions/cache.rs`:
  - two principals with the same visible input share an entry, and a narrower view gets its own;
  - entries hold raw sandbox results only: an envelope refusal is recomputed on each use, and after
    `writes` is widened the same artefact and input succeed with no restart;
  - single flight (two threads, one run); a leader that panics (fault injection) removes its entry
    and wakes its waiters, which retry, and a waiter past `leader_wait_ms` gets `Unavailable`;
  - the per-tenant and process totals; with more tenants than `total / per tenant` (40 tenants),
    the last tenant's second identical read is a hit;
  - the digest memo hits at an unchanged revision and misses after a write;
  - a fork shares entries;
  - a tenant naming a digest compiled for another tenant gets `function_artifact_missing`.
- `functions/runtime.rs`: the RAII guards release the slots and refund the pre-charge on a panic;
  the semaphore hands a freed slot to a third waiting tenant before a busy one; the per-principal
  cap and shares; the write reserve stops reads while an action proceeds.
- `engine.rs`: every acquisition through `CountedRwLock` (read, write, `sync()`, fork) raises and
  lowers the counter, and a runtime call inside `read` panics in debug.
- `action_types.rs`:
  - literal mode renders `{x}` verbatim;
  - the `plan_effects` and `pre_plan` refactor leaves every existing test unchanged;
  - created ids `{kind}:{new_id}-{n}` are stable on re-plan;
  - a created id that exists refuses;
  - until step 10, a function-backed action type refuses with `function: function-backed action
    types are not implemented in this build`.
- `runs.rs`: `retryable` for `Limited("functions: …")` (yes) and `Limited` of anything else (no);
  `execute_run`'s delay is at least the stated refill time.
- `live_schema.rs`: a version holding an op this build does not know (`{"op": "put_widget"}`) fails
  catch-up and `reader` with a storage error, and never loads a partial schema (C.9); renaming a
  property inherited from a parent type rewrites a subtype-qualified `names.props` target.

**onto-core integration** (`tests/functions.rs`, PostgreSQL variants with `TestTenant`):

- **Upload**: insert-if-absent; a known digest returns without validation; the per-principal rate
  and the agent quota; update, delete and truncate refused by the triggers; the quota; the
  `function.artifact` event; corrupt bytes (written by SQL in the test) refused at load; the
  operator purge deletes the row, records `function.artifact.purged`, and calls then refuse
  `function_artifact_missing`.
- **Writer fence**: after `put_function`, a store write at writer version 1 is refused (the pattern
  of tests/changes.rs:1265).
- **Governance**:
  - an agent proposes `put_function` and cannot apply; an approver applies;
  - a code swap is medium risk with the users named, and so is `restricted = "include"`;
  - `changes_between`, revert and undo round-trip functions, a regression test for the glossary
    fallback;
  - a kind, relation and property rename rewrites `names`, and the function still runs;
  - previews appear in `schema.plan`, and as the viewer in `get_proposal` only when asked;
  - an admin files a computed-function proposal over restricted subjects; a member's list and get
    show no subject ids or values;
  - a pre-existing violation (a lowered cap) is a warning, and deleting the function still applies;
  - `function_breaking_change` appears.
- **Actions**:
  - plan and apply with a fixture module; the receipt carries the function block;
  - **inputs changed between plan and commit** (a writer thread edits a walked entity between
    `prepare` and `transact`): nothing is written, it is re-prepared, then committed, or rejected
    after 3 attempts;
  - **definition changed between plan and commit** (another thread swaps the artefact, narrows
    `writes` or re-points the action type between `prepare` and `transact`): nothing commits from
    the old code;
  - nothing moved since `prepare`: the commit skips the rebuild (a counter);
  - **confirmation**: without `expect`, not applied; with a stale `expect`, rejected
    `precondition_changed`; inputs changed but edits unchanged, committed; `ask {confirm}` returns
    a function-backed plan unapplied with the pointer;
  - a delete raises High and needs confirmation, and an agent cannot confirm;
  - a plan that read a restricted item needs confirmation;
  - a caller without the action type's role, and a failed precondition, spend no fuel;
  - empty edits give a rejected receipt with "nothing to change";
  - undo restores, and undo with a moved revision refuses;
  - out of fuel gives a rejected receipt with a `function` check; `functions_off` refuses;
  - a guest error holding a restricted property value, applied by an admin: the receipt holds the
    text, and the `action.<name>` failure event holds only the host text.
- **No guest under a lock** (structural): the counter is 0 at every guest entry in the whole test
  suite (debug assertion), and a debug-only maximum write-lock hold time recorded inside `transact`
  for a writer loop running beside a spinning guest stays within a bound of the same loop without
  guests. Latency targets live in `onto-bench`, not in CI.
- **Computed**:
  - two principals with different visibility get different values, and the same visibility shares
    one entry (hit counter);
  - an edit to a walked entity changes the value;
  - `now:day` changes at the bucket edge only;
  - failures appear in `computed_errors` while `derived` and the entity are returned;
  - the request budget;
  - MCP `get_entity` without `computed` runs no guest;
  - `entity.as_of`, `entity.history` and `map.snapshot` refuse `computed`.
- **Query**: `more`, the output type check with paths, `expect_signature`, the fork in
  `scenario.run`, and a principal over its share gets `Limited` while another principal's call
  proceeds.
- **Tests and drafts**: `ctx.as` with a subject or `--remote` is refused; an explicit input never
  reads the live tenant; an agent runs a draft only when `functions.drafts = "writers"`.
- **Automations**:
  - the closure changes on an artefact swap, so the automation pauses;
  - a run commits with the function block;
  - an unmet condition spends no fuel; empty edits give no row and no receipt;
  - a deterministic fault before the claim is `refused: plan_invalid`, and at commit `failed`;
  - `inputs_changed` is retried;
  - `Limited` is retried at the refill time;
  - out of fuel is not retried;
  - `functions_off` gives a `skipped` row and pauses the automation once; after a resume, rerun
    commits it;
  - deletes are refused;
  - the background reserve holds a run back while an interactive call proceeds;
  - an automation running as admin calls a function whose error holds a restricted prop: a member's
    `events` and `automation.runs` show no guest text.
- **Two `Ontology` instances on one tenant** (PostgreSQL): an upload on A is callable on B; both
  compute byte-identical values at the same `seq`; a kill written on A refuses on B within
  `control_poll_ms`; a tenant whose row lacks a digest that another tenant compiled on A gets
  `function_artifact_missing` on both.

**onto-intelli** (`#[tokio::test]`):

- dispatch of every new action with `{}`;
- the agent matrix: agents may upload (within their quota), test published functions, call and
  propose, and may not disable, enable, kill or approve, with each refusal writing an
  `authorization.denied` event;
- the tenant budget gives `Limited` with the computed retry;
- the semaphore gives `Unavailable`;
- the operator setting turns a tenant off and `function.resume` refuses `functions_off: operator`;
- the denylist file is picked up without a restart and refuses at the next slice;
- shutdown mid-guest: new calls refuse, an in-flight call finishes, and past the deadline it is cut
  and counted in `Released.functions`;
- `estimated_bytes` includes the output cache, and tenant eviction drops it;
- MCP profiles, and `apply_action` with `expect`.

**onto-grpc:**

- upload through HTTP and gRPC at the cap and over it (413);
- cross-tenant upload and call refused;
- `Retry-After` only for the budget;
- HTTP and gRPC error mapping for every class in B.7;
- an `expect` round trip.

**onto-cli** (`crates/onto-cli/tests/fn.rs`, with WAT-generated modules written to temp files):
`build --module`; the build command runs without `ONTO_*` variables and a non-default command needs
`--trust-build`; `test` with fixture cases (pass, fail, determinism, headroom); `--record` against a
local engine; `diff`; `publish --propose`, which refuses when the returned digest differs from the
built file.

**Codegen**: golden files for a fixture schema. In the UI job (Node 22), `tsc --noEmit` compiles a
typed client example, and a misspelt function name must fail.

**SDK tests**: `functions.test.ts`, `test_functions.py`.

**CI additions:**

- a `guest` job (Linux): `rustup target add wasm32-unknown-unknown`, build
  `examples/functions/escalate` with `--locked`, check it is under 200 KiB, that its initial memory
  is under `caps.initial_memory_kib`, that it exports `oi_panic`, and that it passes the module
  rules, and run `cargo test -p onto-core --test functions_guest` against it;
- `cargo deny` covers the new crates (all features, dev dependencies included);
- **osv-scanner** on `Cargo.lock` next to `cargo deny` and `cargo audit`, because GitHub advisories
  such as GHSA-g4v2-cjqp-rfmq never reach RustSec. It is a pinned release binary, not a cargo
  dependency;
- a **weekly differential fuzz job** (scheduled, never per pull request, never on a developer
  machine): a `fuzz/` crate outside the workspace, with its own lock, generates modules within the
  `oi-fn/1` rules with wasm-smith and compares onto-sandbox's results, traps and fuel exhaustion
  against a reference engine. A failure files an issue and blocks the next release, not open pull
  requests. Its dependencies never enter the workspace's Cargo.lock.

The default `cargo test --workspace` does not need the wasm target, because fixtures are WAT.

**Benchmarks** (`onto-bench --functions`, recorded in docs/FUNCTIONS.md before step 16 ships):

- ns per fuel unit (calibration), simple loop and the adversarial mix;
- per-instance cost at 1, 17 and 32 initial pages (the *measured* figures: 2.3 µs at 1 page,
  12.7 µs at 17, 210 µs at 256 and 627 µs at 1024, release);
- input build and digest time under the write lock, at the default and cap sizes, and the share of
  commits that skip it;
- a hub with 1M links, and a principal with 100k `member_of` links: input build time and writer
  p99 while readers build inputs; a hub of degree 100k at depth 2;
- cached and uncached `entity.get` with 8 computed functions;
- an Explorer page of 1,000 rows with one computed column, warm and cold;
- `apply_action` p50 and p99 with a function, against the same edits templated;
- write p50 and p99 with 0 and with 10 computed properties defined (target: no difference).

## 7. Words the docs must use

- "A function runs with the rights of whoever calls it. Approve function code as you would approve
  code your administrators run."
- "A function reads only what its input declares, and only what the caller can see."
- "By default an action function reads no restricted or private item. One that declares
  `restricted = "include"` needs a person's confirmation whenever it read such an item."
- "A function returns edits; the action applies them. A function never writes, and never changes
  who can see what."
- "If the data a function read, or the function itself, changed between plan and commit, nothing
  is written."
- "Text a function writes goes to you, your receipt and traces, never to the event log."
- "Computed values are evaluated on the live graph only; history and as-of reads refuse them."
- "Ids are opaque. Never parse a kind out of one: renaming a kind changes it."
- "Functions share the server process with other tenants. Run them only on hubs whose tenants
  trust each other."
- "A confirmed plan applies only the edits you saw."
- "Limits are counted in fuel, not time: the same call stops at the same point on every server."
- "Computed values are computed when you ask for them and are never stored. They never appear in
  the change feed, webhooks or automation triggers. To subscribe to one, materialise it with an
  automation."
- "A cached value is only ever returned for the exact input it was computed from."
- "A value that could not be computed is reported with its reason, never as empty or zero."
- "A list cut at its limit says `more`."
- "Function budgets and concurrency are per process."
- "Function definitions are visible to everyone who can read the schema. Never put secrets in them."

Never: "real-time computed", "sandboxed network access", "exactly once", or "reversible" for
anything that is not.

## 8. Out of scope for phase 2

- Host imports of any kind: read calls, logging calls, clocks, randomness, WASI.
- Network and LLM calls, secrets, and writeback or webhooks from functions.
- Definer rights and `function:` principals.
- TypeScript (phase 2.1), and Python and Go as supported languages; the component model.
- `CheckKind::Function` preconditions and automation conditions; webhook payload functions.
- Sort and filter on computed values; materialised computed values.
- Packs that ship functions.
- Per-function MCP tools or gRPC methods.
- Artefact garbage collection.
- Metering function fuel to the control plane.
- Reproducible-build verification of `source`.
- Binding confirmation to edits for templated action types.
- Natural-language routing to query functions.
- Functions that run longer than their fuel cap (texc-symphony).
- Payloads on function events, and `link {replace}` in function output.
- Writes to access control (`acl`, `visibility`, `member_of`) from functions.
- Computed values on history and as-of reads.
- Out-of-process isolation per tenant.

## 9. Deferred designs (notes for the next phase)

- **Phase 2.1: TypeScript through `oi-wasi/1`** (owner decision 1).
  - **The import set.** A second rule set in `onto-sandbox` implements a deterministic WASI
    preview 1 subset in-house: not `wasmi_wasi`, which pulls `wasi-common` and `wiggle`.
    - `fd_read(0)` reads the same input document;
    - `fd_write(1)` writes the output (capped) and `fd_write(2)` the log (4 KiB);
    - `fd_fdstat_get` and `fd_close` cover fds 0-2;
    - `clock_time_get` returns the input's bucketed `now`, or 0 with a `clock_read` flag that
      `function.test` reports;
    - `random_get` draws from a pinned PRNG seeded from the input digest;
    - the `environ_*` and `args_*` calls return empty;
    - `proc_exit` ends the call.

    The exact list is fixed by inspecting what current Javy, Rust `wasm32-wasip1` and TinyGo
    outputs import, and is asserted in a test.
  - **Authoring.** Authors build Javy *static* modules themselves (about 870 KB, within the 2 MiB
    cap) with a guest SDK `@onto-intelli/functions` (builders, edits, `fail()`), with TS guest
    bindings from `function.types {lang: "ts", for: "guest"}` and the JS source attached as
    `source`.
  - **The alternative.** A vendored, pre-initialised Javy plugin, with the JS source as the stored
    artefact so reviewers read a true source diff. It is better for review but adds a ~1 MB blob
    to the repo, depends on Javy's plugin API, and needs a reproducibility job.
  - **Gate before shipping.** A warm trivial call p50 ≤ 5 ms and a page of 50 computed values
    ≤ 60 ms, cold, on the M2 Pro. If it misses, design instance reuse with memory snapshots first.
- **`oi-fn/2`: read imports**, for data-dependent exploration.
  - Imports `oi_get`, `oi_neighbors` and `oi_find`, each taking one short `reader()` guard per call
    (never across guest execution, which the guard counter enforces), charging fuel per entity, and
    returning sorted canonical JSON.
  - A read log of `(call, sha256(result))` is replayed natively at commit and compared, as the
    input digest is now.
  - Cache keys become the read log's hash chain.
- **Binding confirmation for templated actions.** `expect = plan digest` for every action type.
- **Validation functions.** `CheckKind::Function` returning `{ok, message}`, usable as
  preconditions and automation conditions. They share the input builder and are re-checked by
  input digest at commit.
- **Materialised computed index.**
  - A `computed_values(def_hash, entity, value, as_of_seq, input_digest)` table, maintained by a
    change-feed consumer (lease, cursor, fence, as in phase 1).
  - It is served only when a reader's view covers the entry's observed set.
  - It enables sort and filter at scale, but needs definer-rights principals.
- **Explorer sort and filter on computed values**, at most `max_sort_candidates` candidates and
  refused above, once the materialised index or measured costs justify it.
- **Artefact GC.** Deletes artefacts that no schema version, proposal or recent upload names after
  a TTL, using the `onto.retention` escape of `refuse_delete` (0008:7-13).
- **Metering.** `UsageMeter.record_functions(calls, fuel)` to the control plane's `usage_daily`, an
  additive onto-control migration `0004`, plus an optional `monthly_fuel` cap. Billing is a product
  decision.
- **Definer rights.** A `function:<name>` reserved prefix, Tutela-minted principals, dominance at
  approval and an enablement TTL.
- **wasmtime backend.** `=48.x` LTS with no default features behind the same `onto-sandbox` API,
  only if a measured workload is compute-bound. It costs about 1.6 GB of target, an MSRV bump to
  1.95 and an advisory routine. Fuel stays the governed limit.
- **Process isolation** (`[functions] isolation = "process"`). Each tenant's guests run in a child
  worker process, spawned per tenant with rlimits (address space, CPU time as a backstop only, no
  file descriptors beyond its pipe), speaking the same JSON documents over a pipe. onto-sandbox is
  the only crate that changes. **Trigger:** a hub that must host mutually untrusting tenants with
  functions on, or a second interpreter memory-safety advisory affecting the pinned version. Until
  then such hubs keep functions off (A.8).
- **Typed event payloads.** `writes.events.<suffix> = {fields}` whose values must equal properties
  of the event's target as of the commit, so a payload reveals nothing the target does not.
- **Per-relation adjacency.** An index from `(entity, relation)` to links would make a walk's scan
  proportional to the relation's degree rather than the node's. It is a graph.rs change that every
  traversal would benefit from, and is weighed when step 16's hub benchmark shows the scan cap
  binding on real schemas.
- **A replica registry** recording each live replica's writer version, so `put_function` could be
  refused while older writers are live (C.9).
- **Shared cache tier.** A PostgreSQL `function_cache(key, value, created_at)` for expensive
  functions. Content addressing means it needs only a TTL vacuum.
- **Python** through componentize-py, once the engine runs components.

## 10. Decisions that need the owner

1. **Authoring languages in phase 2.**
   - (a) Rust only through the zero-import `oi-fn/1`, with TypeScript as the phase 2.1 slice through
     an in-house `oi-wasi/1` import set and author-built Javy modules.
   - (b) TypeScript inside phase 2 through a vendored Javy plugin, with JS source as the artefact.

   *Recommended: (a).* TypeScript is the language most authors expect, but it doubles the host
   surface, depends on Javy APIs nobody here has verified, and runs QuickJS inside an interpreter,
   whose speed must be measured first. Phase 2's ABI documents carry over unchanged, so (a) loses no
   work.
2. **Runtime.** wasmi, an interpreter: 7 crates, a 130 MB debug target, deterministic, and about
   5-10x slower than Cranelift on compute. Or wasmtime 48 LTS: faster, about 1.6 GB of target,
   MSRV 1.95, and frequent advisories against `ignore = []`. *Recommended: wasmi*, with portable
   dispatch, its interpreter crates optimised in debug builds, GitHub-advisory scanning and a
   weekly differential fuzz job. An interpreter is a smaller surface, not a safe one: wasmi had a
   linear-memory use-after-free in 0.41-1.0 (CVE-2025-66627), and the 2.0 executor is unaudited.
   Functions are bounded and short, the cache absorbs repeats, and the platform values a small
   attack surface over raw speed. The backend stays swappable behind `onto-sandbox`, and process
   isolation is the planned answer for hubs with mutually untrusting tenants.
3. **Agents uploading artefacts.** Uploads are inert until a person approves a schema change naming
   the digest. *Recommended: allow* (`function.upload` in `AGENT_WRITES`, MCP `propose_function`,
   within the tenant quota and a smaller per-agent quota), so agent-authored functions can be
   proposed. Agents do not run drafts unless the project allows it (`functions.drafts`). Agents still never apply,
   approve, enable, disable or kill.
4. **Invoker rights only.** Phase 2 has no definer-rights functions, which would need
   `function:<name>` principals and a Tutela contract change. *Recommended: invoker rights only*,
   revisited when Tutela issues service principals. Until then, a value that must be the same for
   every reader is materialised by an automation (E.7).
5. **"Reads the ontology" means declared inputs in phase 2.** There are no host-call reads: a
   function cannot follow an id it found in a property, or chain searches, beyond its declared
   walks. *Recommended: yes.* Declared inputs are what make no-invalidation caching, the native
   commit check, reviewable reads and exact budgets possible. `oi-fn/2` (section 9) adds read
   imports later.

Engineering choices settled here rather than escalated:

- no cargo feature for the sandbox, and functions off by default;
- `portable-dispatch`, and the interpreter optimised in debug builds;
- fuel as the only limit, with no wall-clock class, and instantiation and input scans charged in it;
- Eager compilation;
- a fresh instance per call, never batching;
- content-addressed caching of sandbox results only, with single flight;
- artefacts kept forever in phase 2, behind triggers, with an operator purge;
- the writer fence, and every replica upgraded before the first function;
- computed values opt-in, in their own map, and current only;
- confirmation bound to edits for function-backed plans, and to a person for restricted reads;
- guest text kept out of events and run rows;
- per-process budgets with principal shares and background and write reserves;
- per-project overrides through project settings, including the operator switch;
- the digest denylist and quarantine.

## 11. Build order

One engineer, sequential, one cargo build at a time (each `target/` costs 10-100 GB on this Mac).
Every step ends green on `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets --locked -- -D warnings`, plus the tests the step names
(PostgreSQL variants with `DATABASE_URL=postgres://onto:onto@127.0.0.1:5441/onto`). Run `make test`
before each commit. Any step that changes dependencies also runs `cargo deny --locked check`.

0. **Rebase.** PR #8 is merged and this branch's base is an ancestor of `origin/main`, which is 30
   commits ahead. Rebase `feat/phase2-functions` onto `origin/main`, re-check every line reference
   this document makes in engine.rs, engine_sync.rs, store/, policy.rs, action_types.rs, runs.rs and
   live_schema.rs, and run `cargo deny --locked check` on the rebased lock (main added `quick-xml`).
   main's migrations still end at 0014, so 0015 is free; if main gains a migration first, renumber
   here before merging.
1. **Sandbox crate.**
   - Files: `crates/onto-sandbox/` (`Cargo.toml` with `portable-dispatch`, `lib.rs`, `rules.rs`,
     `limiter.rs`, `run.rs`), the workspace `members`, the `[profile.dev.package.*]` overrides
     (A.1), Cargo.lock.
   - Behaviour: the own limiter, the instantiation charge, `catch_unwind` and `Fault::Internal`,
     `oi_panic`, the initial-memory rule.
   - Tests: the onto-sandbox unit list (section 6), including the debug-profile dispatch tests on all
     three OSes, sliced against unsliced across slice sizes and grows, and the cross-OS vector.
   - Gate: `cargo deny --locked check` is clean; `cargo test -p onto-sandbox` passes in debug; the
     commit message records the clean-build time and the `target/` growth for normal and dev
     dependencies (`wat`, the opt-level override), and the calibration: ns per fuel for the simple
     loop and the adversarial mix, in release and in the debug test profile. The fuel caps and
     defaults in J are reset from the worst mix here (A.5).
2. **Guest crate and example.**
   - Files: `sdk/rust-fn/` (crate `onto-fn`, with the panic hook and `oi_panic`),
     `examples/functions/escalate/` (with the 64 KiB stack setting), workspace `members`, and the CI
     `guest` job.
   - Tests: `onto-fn` host-side unit tests (JSON types, `entry!` dispatch, the panic hook) and the
     example's handler tests on the host. The guest job loads the built module into `onto-sandbox`.
   - Gate: the example is under 200 KiB, its initial memory is under the cap, and it passes the
     module rules.
3. **Groundwork with no behaviour change** (two commits).
   - (a) `CountedRwLock` for the graph, text and vector locks in `engine.rs`, with every direct
     acquisition (section 2) going through it.
   - (b) `pre_plan`, `plan_effects` and `Templates::literal` in `action_types.rs`.
   - Tests: the counter unit tests. Every existing test passes unchanged.
   - Gate: `cargo test -p onto-core` with no test edited.
4. **Schema section, definitions and the writer fence.**
   - Files:
     - onto-core `functions/{mod,def}.rs` (including `restricted`, `limits.scan` and the
       access-control refusal);
     - `schema.rs` (`Schema.functions`, `SchemaFile`, `extend_toml`, `compile`,
       `ObjectType.computed`, and the `derived` doc comment);
     - `action_types.rs` (`ActionType.function`, `validate`, `primary_param`, and the plan check
       `function: function-backed action types are not implemented in this build`, removed in
       step 10);
     - `live_schema.rs` (the ops and every list in C.4, renames rewriting `names` with subtype and
       interface targets);
     - `locks.rs`, `packs.rs` names, the `automate.rs` closure item;
     - the fence, because this step introduces the first shapes older builds cannot read:
       `store/mod.rs` (`WriteBatch.min_writer`), `store/postgres.rs` (`WRITER_VERSION = 2`, the
       `GREATEST` update), `MemoryStore`, and `Extra` carrying `min_writer` from `schema_commit` and
       `record_version`.
   - Tests: the def unit tests, unchanged fingerprints and `hash_of`, `changes_between`, revert and
     undo, renames (an inherited property included), closure staleness on a definition change, the
     not-implemented plan check, the `min_writer` refusal, and the unknown-op catch-up failure (C.9).
   - Gate: `cargo test -p onto-core` and the test-postgres variants.
5. **Artefacts and governance.**
   - Files:
     - `migrations/0015_function_artifacts.sql` and the `MIGRATIONS` entry;
     - `store/mod.rs` (`WriteBatch.artifacts`, `Store::artifact`, `artifact_bytes`), `MemoryStore`;
     - `Extra.artifacts` and `Ontology::upload_artifact` (hash first, known digests, per-principal
       rate and agent quota);
     - `check_functions` in its two parts (before and inside the lock, change-set relative), risk
       reasons, the static lint rows;
     - the operator purge action.
   - Tests: the PostgreSQL triggers, the quotas and rate, events, plan refusals, a pre-existing
     violation as a warning, propose by an agent, the purge, migrating a pre-0015 tenant.
   - Gate: the test-postgres variants.
6. **Input builder and output decoder.**
   - Files: `functions/input.rs` (scan cap, `restricted`, `canonical_json`), `functions/output.rs`
     (allowed targets, no `replace`, no event payload, host-written violations, text sanitising),
     `TypeDef` validation.
   - Tests: the visibility and restricted matrix, the scan cap, permutation determinism, `more` and
     `on_limit`, the decoder rules and the property test.
   - Gate: `cargo test -p onto-core functions::`.
7. **Runtime handle.**
   - Files:
     - onto-core `functions/{runtime,cache,stats}.rs` (`FnHost`: module cache gated by verified
       digests, round-robin semaphore, sliced run, stop flags, denylist file, quarantine,
       output-cache registry; `FnTenant`: bucket with principal shares and both reserves, RAII
       charges and slots, switches with lazy refresh and the operator setting, sandbox-result cache
       with fair-share eviction and RAII single flight, the digest memo, stats);
     - `OntologyOptions.functions`, `fork()` sharing, `CONFIG_NAMESPACES`;
     - onto-intelli `config.rs` `FunctionsConfig` and the template, Hub wiring, `estimated_bytes`,
       the project-settings limits in onto-control `limits_of` (0 as zero for functions),
       `TenantSettings.functions`, `Released.functions`, the startup memory sum and the Helm sizing
       comment;
     - `onto-bench --functions calibrate`.
   - Tests: cache keys and raw-result entries, single flight with a panicking leader, the 40-tenant
     fair share, cross-tenant digests, budget `Limited` with the retry, principal shares, both
     reserves, semaphore `Unavailable` and round-robin, kill, off, operator and denylist at a slice,
     quarantine, shutdown cut.
   - Gate: onto-core and onto-intelli unit tests; the calibration number in the commit message.
8. **Query functions, `function.test` and `function.input` (engine side).**
   - Files: `Ontology::call_function` (signature, `expect_signature`), `functions/test.rs` (the
     fixture runner shared with the CLI; `ctx.as` and explicit inputs bound to fixtures; the drafts
     setting), `Ontology::function_input`.
   - Tests: the query cases, fixture cases, the double run, headroom, the test and draft refusals.
   - Gate: `tests/functions.rs` query and test cases.
9. **Computed properties.**
   - Files: `Ontology::computed` (chunks with a scan budget, the digest memo), the `entity.get`
     `computed` option, `entity.computed`, request budgets, `objects.explore {computed}` page columns
     (explorer.rs `Field`), and the `computed` refusal on `entity.as_of`, `entity.history` and
     `map.snapshot`.
   - Tests: the computed list, and the structural "no guest under a lock" test.
   - Gate: `tests/functions.rs` computed cases; write p50 and p99 unchanged with 10 computed
     properties defined.
10. **Function-backed actions.**
    - Files: `PlanSeed.function`, `FunctionRun` (with `def_hash`, `revision`, `scanned`, `detail`,
      `restricted_read`), `prepare` (with `pre_plan` first), the `apply_action`, `plan_action` and
      consequences loops, the `apply_planned` definition and digest checks with the revision skip,
      `ActionPlan.function`, `ApplyOptions` `expect` (`Copy` dropped) at every construction site
      (onto-intelli actions.rs:976 and 1411, command.rs:146, seed.rs:387; onto-core
      action_types.rs:1770, modeling.rs:2172, learn.rs:907; tests/{writeback,execution,packs,v02,
      action_plans}.rs), the `ask` refusal, host-written check text, and the removal of step 4's
      not-implemented check.
    - Tests: the action cases, including the inputs-changed race, the definition-changed race, the
      revision skip, the confirmation-binding cases, restricted reads and the event-leak case.
    - Gate: the onto-core action tests plus the `tests/functions.rs` action cases.
11. **Automations.**
    - Files: `runs.rs` (`prepare_run` with conditions first in `evaluate_run` and `commit_run`, empty
      edits as `NotMet`, the `retryable` rule, the `execute_run` delay), the consumer's handling of
      `functions_off` (`skipped` rows and one pause) and of pre-claim faults, the background flag,
      lint `action_writes` reading the envelope for `automation_loop`, `run_trace` spans.
    - Tests: the automation cases, including the leak test with an admin `run_as`.
    - Gate: the onto-core and onto-intelli automate tests.
12. **Plan previews and data lints.**
    - Files: `SchemaPlan.functions` (computed in `schema_plan_as`, guests after `drop(g)`),
      `without_reader_facts` and `with_reader_view` covering `functions`, `get_proposal
      {preview_functions}` with the per-viewer preview cache, `function_breaking_change`,
      `function_input_wide` (counts only), `function_unused` without query functions.
    - Tests: previews as the planner and on request as the viewer, nothing stored, the restricted
      proposal case, `skipped` reasons, `preview_fuel` charged to the viewer.
    - Gate: the `tests/functions.rs` governance cases.
13. **Engine surfaces.**
    - Files: onto-intelli `functions.rs` (`CALLS`, `AGENT_WRITES`, `GRAPH_READS`, traces, stats,
      health with the operator setting, denylist and quarantine), the purge admin route, `mcp.rs`
      tools, profiles, the `get_entity.computed` argument and its description, and `apply_action`
      `expect`, onto-grpc proto `FunctionService` and `BUILTIN_ROUTES`, `http.rs` routes with the
      upload body limit.
    - Tests: dispatch, the agent matrix, MCP profiles, upload limits on both transports,
      cross-tenant refusals, error mapping, the operator and denylist cases.
    - Gate: workspace tests.
14. **Author toolchain.**
    - Files: onto-core `functions/codegen.rs` (`function.types` for Rust guest and TS client, with
      no kind-prefix helpers), `crates/onto-cli/src/functions.rs` (`onto fn …`, the scrubbed build
      environment, `--trust-build`, the digest check in `publish`), `onto do` `expect`.
    - Tests: golden codegen files, and the CLI integration tests with WAT-generated modules.
    - Gate: workspace tests and the `guest` CI job (the example built through `onto fn build` and
      tested through `onto fn test`).
15. **SDKs and UI.**
    - Files: the `sdk/typescript` and `sdk/python` `functions` namespaces, `expect` in
      `actions.apply`, and the UI (Functions page, test console, Proposals diff and on-request
      previews, ActionDialog `expect` and the labelled message, the EntityCard "Derived" rename and
      new Computed section, Explorer computed columns), with `crates/onto-grpc/ui-dist` rebuilt.
    - Tests: the SDK tests, the UI build, `tsc` on the typed client example.
    - Gate: the CI `ui-dist` diff is clean.
16. **Benchmarks, CI and docs.**
    - The full `onto-bench --functions` run (section 6), with the numbers recorded, including the
      hub and `member_of` benchmarks.
    - CI: osv-scanner next to `cargo deny`, and the weekly differential fuzz job.
    - Docs: `docs/FUNCTIONS.md` (the section 7 wording, ABI, limits, fuel calibration, the Rust
      quickstart, testing, the materialisation pattern, the bench numbers, the wasmi bump procedure,
      the untrusting-tenants warning), STORAGE.md (0015, the writer fence and the every-replica rule,
      rollback steps, the purge runbook), the README capability table, CONTROL_PLANE.md (routes,
      project-settings limits, the operator switch) and ARCHITECTURE.md (the TCB in A.8).
    - Gate: the demo eval on PostgreSQL still passes; the fuel defaults are set from the
      calibration.

## 12. Remaining open questions (not owner decisions; settle during the build)

- Are the default fuel values (50M for action and query, 10M for computed) and the caps right? Set
  them from step 1's worst-mix calibration and the example functions.
- `slice_fuel` at 10M: is the per-slice overhead (one resume and a few atomic loads, the denylist
  check included) under 1%? Measure in step 1.
- Are the scan defaults (20,000 per input, 100,000 cap) and `scan_fuel_per_edge = 50` right? Set
  them from step 16's hub benchmarks; the per-relation adjacency index (section 9) is the remedy if
  the cap binds on real schemas.
- Are the principal shares (50%, agents 20%) and `max_concurrent_per_principal = 1` right for apps
  that call through one project key? Watch `Limited` refusals by principal in stats; projects raise
  them in their settings.
- Is a minute the right finest bucket for action time? A confirmation that crosses a bucket edge
  may refuse "edits changed", which is honest but may annoy. Watch the refusal rate.
- Should `more` also carry a lower bound on the visible count (`at_least`)? Phase 2: no.
- Hashing large pages: is SHA-256 per subject material next to assembly on 1,000-row Explorer
  pages? If so, hash incrementally while serialising. The digest memo (E.2) removes it at an
  unchanged revision.
- Is `restricted = "exclude"` too strict for common action functions? Watch how often definitions
  declare `include`, and how often their runs are refused for needing a person.

Settled by the review (revision 2): tail-call dispatch is never used (`portable-dispatch`, A.1);
input building is charged in fuel (B.2); the commit skips the rebuild when `revision()` has not
moved (D.3).

## 13. Synthesis record: grafts, conflicts and corrections

**Base.** security-minimal, chosen over author-first (section 1, "How the base was chosen").
scale-ops supplied the operations layer; author-first supplied the author layer.

**Grafts**

| From | What | Where |
|---|---|---|
| scale-ops C.7 | `WRITER_VERSION = 2` and the `min_writer` fence instead of documentation only | D15, C.9 |
| scale-ops D.6 | the `retryable` rule for `Limited("functions: …")` at the refill time; `functions_off` final | D19, D.6 |
| scale-ops A.6 | the thread-local guard counter with a debug assertion | D5, A.6 |
| scale-ops E.3 | single flight, the process-wide value-cache total, cache bytes in `estimated_bytes` | D12, E.3 |
| scale-ops E.4 / author-first F | computed values opt-in on MCP and `EntityCard`, per-request fuel and count budgets | D17, E.4, E.5 |
| author-first D14 | confirmation bound to `edits_digest` via `expect` | D11, D.4 |
| author-first D15 / scale-ops E.1 | a sibling error map (`computed_errors`) instead of `_unavailable` inside `derived` | D17 |
| author-first A.6 | the fixture-graph test runner, double run, headroom, `--record`, the test console | G.4 |
| author-first D.5 / scale-ops C.5 | old-against-new previews in plans and proposals, `skipped: <why>`, the displayed `source` | C.5 |
| author-first D.2/D.4/D.9 | the signature hash, `expect_signature`, `function_breaking_change` | C.6, F |
| author-first A.4 | `function.types` codegen (Rust guest first; TS client now; TS guest in 2.1) | G.2 |
| scale-ops A.4 | fuel run in resumable slices for kill, off and shutdown, with sliced equal to unsliced | D21, A.4 |
| scale-ops D.6/H.1 | `background_reserve_pct` on the tenant bucket | D20, D.6 |
| scale-ops E.6/C.8 | the materialisation pattern and `function_input_wide` (`function_writes_trigger` was dropped in revision 2: `automation_loop` covers it) | E.7, C.8 |
| scale-ops G | `function.input`, bounded per-replica per-function stats | G.5, 5 |
| scale-ops A.2 | `enforced_limits` for parse bombs; the engine id in cache keys | A.2 |
| author-first C.3 | typed query outputs with path errors; `entity` values must be input ids | C.3, F |
| author-first A.7 | `onto fn diff` and `onto fn publish` (propose by default) | G.6 |
| author-first A.3 | a function-supplied `message` shown with the plan | B.4, D.4 |
| scale-ops C.3 | computed properties in their own `[object_types.<t>.computed]` map, not in `DerivedDef` | D17, E.1 |

**Conflicts resolved**

| Question | Options in the drafts | Decision and why |
|---|---|---|
| Wall-clock limit | scale-ops: a `timeout` class at slice boundaries (`Unavailable`, not cached); one judge: none; the other: optional | **None.** A timeout is a nondeterministic transient path; fuel caps already bound time (set from calibration to at most about 5 s on the worst mix, A.5), and slicing gives preemption for kill and shutdown without it. |
| Compilation mode | scale-ops: `LazyTranslation`; security-minimal: `Eager` | **Eager**: lazy translation charges fuel on first use (code_map/mod.rs:860-871), which breaks fuel determinism and negative caching. |
| Batching computed calls | author-first: up to 64 subjects per instance | **Never**: a fresh instance per call. Batching lets guest state carry one reader's data into another's cached value, and only a test checks independence. |
| `find` in action inputs | author-first: allowed | **Refused**: the commit rebuilds the input under the write lock, and a `find` would scan a kind there. |
| Over a walk limit | author-first: error; security-minimal: cut with `more` | **Cut with `more` by default, `on_limit = "error"` per binding**: honest, and it does not break as data grows. `function_input_wide` warns. |
| Edit targets | scale-ops: anything within the envelope | **Input entity ids only** (revision 2 narrows this to the `id` fields of entity objects, with no `replace`): no blind writes, and the commit digest covers every target. |
| Pinning confirmations | author-first `expect = edits_digest`; scale-ops `expect_inputs` | **`edits_digest`**: it refuses only when what the person saw actually changes, and a retry with moved inputs but the same edits still commits. |
| Where computed values go | security-minimal: `DerivedDef.function` in `derived`; others: separate | **A separate `computed` map, opt-in**: MCP forces `derived`, and a typed `derived` shape stays clean. |
| Artefact deletion | author-first: `artifact_delete`; scale-ops: TTL delete; security-minimal: never | **Never in phase 2**, with refuse-delete and no-truncate triggers. `refuse_delete`'s retention escape allows a later GC without a migration. |
| Renames | scale-ops: refuse unless the function is re-put; author-first: property `as` plus a warning | **The alias table**, which the existing rename passes rewrite for kinds, relations and properties. |
| Engine id in hashes | scale-ops: in `def_hash`, hence in closures | **In cache keys and receipts, not in closures**: a wasmi bump changes fuel costs, not approved code, and must not pause every automation. |
| Explorer | scale-ops: columns, plus sort and filter up to a cap | **Page-only columns now; sort and filter deferred**: the latter is a different cost class, and materialisation covers it with governed writes. |
| Metering and tenant limits | scale-ops: `UsageMeter` plus control-plane migration 0004 | **Tenant limits through project settings now (no migration); metering deferred**: billing is a product decision. |
| Cargo feature | scale-ops: a `functions` feature in the default set | **None**: CI tests default features only, and a "not built" refusal path is untested code. |
| Default state | author-first and scale-ops: enabled | **Off by default**, like `[writeback]`. |
| Code at upload | author-first: runs `describe` at upload | **No code runs at upload**: definitions are TOML, and the module is only validated. |
| Exact `now` for actions | security-minimal: receipt time | **Buckets only**: an exact clock would make bound confirmations refuse on almost every retry, and would split caches. |

**Corrections to the drafts found while verifying**

- security-minimal B.7 said a spent tenant budget is "retried by timer". `runs::retryable`
  (runs.rs:477-483) never retries `Limited`. Fixed by the new rule (D.6).
- security-minimal C.7 left the phase-1 silent drop of the `functions` section to documentation.
  `head.min_writer` can fence it. Fixed (C.9).
- The config-mode refusal is at live_schema.rs:3594, not 3606. `parse_params` is at onto-cli
  main.rs:900; 2479-2524 is the `Do` arm that uses it.
- `explore` is at explorer.rs:559 and `Field` at 206; 48 is in the spec type. `system.health` is
  served at onto-intelli actions.rs:1449 from `outbound_health` (intelli.rs:508).
- `wasm_simd`, `wasm_relaxed_simd` and `wasm_memory64` exist only with their wasmi features
  (config.rs:296-335). With those features off, the proposals are absent, so no setting is needed
  and none would compile.
- `inner_graph_read` (engine.rs:701) is a third guard path, used by authority.rs and learn.rs, that
  the guard counter must cover besides `read` and `reader`.
- runs.rs already has a type named `Prepared` (391-398), so the function output type is
  `FunctionRun`.
- author-first and scale-ops quoted 24 crates and 282 MB for wasmi with default features. The
  measured lean configuration is 7 crates and 130 MB.
- The per-effect loop of `plan_on_as` spans action_types.rs:1065-1310, and `PlanSeed` is at 507-525.

---

## Review decisions

Revision 2 answers a three-lens review: security (SEC-1 to SEC-19), governance (F1 to F13) and
compatibility and performance (F1 to F10). The two F series reuse ids, so each entry names its lens.
Each finding was checked against the worktree, origin/main, the wasmi 2.0.0 sources or the GitHub
advisory database before deciding. Findings that two lenses raised share one resolution.

1. SEC-1, guest text and restricted data reach workspace-wide events — **Adjust**: confirmed
   (`reject()` copies check text into `action.<name>`, the run reason goes into `automation.run`,
   and main's `a10d5b6` filters events by target, never by payload). Plan-check text is now
   host-written (D25, B.7), event edits carry no payload (the first option of part 2), and action
   functions default to `restricted = "exclude"`; a restricted entity *parameter* is refused rather
   than dropped, so a function never computes over a set it believes complete. The leak test is in
   section 6.
2. SEC-2, `default-features = false` selects tail-call dispatch — **Accept**: confirmed in
   dispatch/mod.rs and build.rs, and measured (a stack overflow at 3,000 loop iterations in debug).
   `portable-dispatch` is set, A.1 and D1 are corrected, the 1 ns figure is replaced by the measured
   1.3 ns with the final features, and step 1 calibrates an adversarial mix from which the caps and
   the shutdown arithmetic are derived.
3. SEC-3, `trap_on_grow_failure` breaks sliced fuel — **Accept**: confirmed (limiter.rs:160-165,
   exec.rs:427-431); onto-sandbox's own limiter lets a grow's fuel shortfall take the resumable
   path, and step 1 compares outcomes across `slice_fuel` 1,000, 10M and unlimited, grows included.
4. SEC-4, old-against-new previews stored in proposals — **Accept**: confirmed
   (`without_reader_facts` strips only consequences and shapes); `functions` is stripped before
   storage and computed per viewer, `function_input_wide` reports counts only, and a test files an
   admin proposal over restricted subjects.
5. SEC-5, `replace` and a loose "appears in the input" — **Accept**: `replace` is removed from the
   vocabulary, allowed targets are the `id`s of entity objects in the host-built input (`unlink.to`
   included), and `function.test` with an explicit input plans only against its fixture or an empty
   graph.
6. SEC-6, input building is uncharged work under the write lock — **Accept**: confirmed (one
   adjacency list per entity; `steps_via` filters the whole degree). `limits.scan` and `caps.scan`
   with one fixed refusal text, scanned items charged in fuel, the commit rebuild bounded by twice
   the prepared scan, `find` bounded the same way, and hub and `member_of` benchmarks; the
   per-relation index is in section 9.
7. SEC-7, no fairness between principals in a tenant — **Accept**: per-principal shares (smaller
   for agents), a write reserve below which reads stop, a per-principal concurrency cap (default 1,
   raised per project for apps on one key), round-robin hand-out by tenant, previews charged to the
   viewer and cached per (proposal revision, scope key), and uploads hashed first and rate-limited.
8. SEC-8, the trusted computing base was overstated — **Adjust**: the CVE, the audited versions and
   the unaudited 2.0 executor are stated (A.8, D1, section 10); osv-scanner joins `cargo deny`; a
   bump procedure is written; drafts are off for agents by default; previews of unapproved code run
   only on request; untrusting hubs keep functions off; and process isolation is in section 9 with
   a trigger. The differential fuzz job runs weekly in its own crate and lock rather than on every
   pull request, because a reference engine would add gigabytes of build to each PR and to this Mac.
9. SEC-9, a panicking leader wedges single flight and leaks slots — **Accept**: `catch_unwind`
   gives `Fault::Internal` (never cached, counted toward quarantine); slots, charges and in-flight
   entries are RAII guards whose `Drop` wakes waiters; and waiters wait at most `leader_wait_ms`, a
   transient capacity bound rather than a time limit on the function.
10. SEC-10, the shared module cache is keyed by digest alone — **Accept**: each tenant keeps a
    verified-digest set filled only from its own row, checked on every path before the shared
    cache.
11. SEC-11, no operator-owned switch — **Accept**: a control-plane-only `functions.enabled` that
    `function.resume` cannot undo, and a process-wide denylist read from an operator file (chosen
    over an admin route because one ConfigMap reaches every replica), both shown in
    `system.health`; `0` is a real zero for function limits.
12. SEC-12, the guard counter misses direct lock sites — **Adjust**: confirmed (about a dozen
    direct sites plus `sync()`). The graph, text and vector locks become a private
    `CountedRwLock`, so an uncounted acquisition cannot be written; the clippy `disallowed_methods`
    entry is dropped because it would flag every unrelated parking_lot lock and adds nothing once
    the raw lock is private.
13. SEC-13, functions could change access control — **Accept** (the refusal option): `compile()`
    refuses `writes` aliases targeting `visibility`, `acl` or `ACL_LINK_TYPES`, so no risk floor is
    needed.
14. SEC-14, `ctx.as` against the live tenant — **Accept**: `as` is valid only with a graph
    fixture; live inputs are always built as the authenticated caller.
15. SEC-15, format characters in `message` — **Accept**: Cf characters are stripped and text is
    NFC-normalised (both crates are already in the lock), and the message is rendered after the
    preview lines in a labelled style.
16. SEC-16, the output-cache total starves later tenants — **Accept**: an insert over the total
    evicts from the tenant most over its fair share, with a 40-tenant test (shared with
    compat-perf F4).
17. SEC-17, `onto fn build` runs an agent-chosen command with the user's token — **Accept**:
    `ONTO_*` and token variables are removed from its environment, a non-default command needs
    `--trust-build`, publish checks the uploaded digest against the built file, and the proposal
    note records the command.
18. SEC-18, no takedown path for artefacts — **Adjust**: the purge is a hub admin action rather
    than hand-run SQL, so the `function.artifact.purged` event goes through the engine in the same
    transaction as the delete; the runbook adds the digest to the denylist, and agents get a
    per-principal upload quota.
19. SEC-19, canonical bytes rely on serde_json's map type — **Accept**: inputs, digests and cache
    keys use `canonical_json`, which sorts keys itself, with a unit test; section 2's claim is
    corrected.
20. F1 (governance), the commit checks only the input digest — **Adjust**: `FunctionRun` gains
    `def_hash`, and a changed function or definition is `inputs_changed` (back to `prepare`, where
    `expect` then guards the new edits); a switch that is now off is a final `functions_off` check
    instead of a retryable conflict, since retrying cannot help while it is off.
21. F2 (governance), cached refusals outlive the definition — **Accept**: the cache holds only raw
    sandbox results and faults; decoding and the envelope, alias, type and id checks run on every
    use; the created-id check is a plan check; B.7's Cached column says so; and a test widens
    `writes` and succeeds without a restart.
22. F3 (governance), the mixed-version claim was wrong — **Adjust**: C.9 and D15 now say a phase-1
    replica can neither catch up on nor read such a tenant and fails loudly, and a test pins that
    with an unknown op. The `system.health` refusal of `put_function` is not adopted, because no
    replica registry exists to know that older writers are live (section 9), so upgrading every
    replica first is a documented hard requirement.
23. F4 (governance), the guest runs before roles, preconditions and conditions — **Accept**:
    `pre_plan` (roles, the write right, parameters, preconditions) runs under the input's read guard
    before any guest, and `prepare_run` evaluates conditions first; tests assert zero fuel.
24. F5 (governance), `ask` and MCP cannot carry `expect` — **Accept**: MCP `apply_action` gains
    `expect` and unapplied results carry `edits_digest`; `ask {confirm}` returns function-backed
    plans that need confirmation unapplied, with a pointer, because its confirm re-runs the model
    and cannot bind to a preview; every `ApplyOptions` site is listed in step 10.
25. F6 (governance), automation outcomes were misstated — **Adjust**: pre-claim faults are
    `refused: plan_invalid` and commit-time faults `failed`; empty edits are `NotMet` for
    automations and "nothing to change" for applies (stated in B.4). For `functions_off` the
    automation is paused once (the pause option), and the run is recorded as `skipped` so
    `automation.rerun` can replay it after a resume, which keeps a materialisation from losing
    updates silently.
26. F7 (governance), guard counter coverage — **Adjust**: the same resolution as SEC-12; the
    newtype makes the proposed CI grep unnecessary.
27. F8 (governance), `function_writes_trigger` duplicates `automation_loop` — **Accept**: dropped;
    the envelope feeds `action_writes`, E.7's loop guard cites `allow_self` and `automation_loop`,
    and `function_unused` skips query functions.
28. F9 (governance), renames reach ids and subtype targets — **Accept**: ids are documented as
    opaque with no kind-prefix helpers, recorded inputs are re-recorded after a kind rename, and the
    `RenameProperty` arm rewrites subtype and interface targets, with a test.
29. F10 (governance), "Computed" already names derived values — **Accept**: EntityCard's section
    becomes "Derived", and mcp.rs:98 and schema.rs:218 are reworded, each in the step that adds
    `computed` to that surface (steps 4, 13 and 15).
30. F11 (governance), as-of reads silently omit computed values — **Accept**: computed values are
    current only, and `entity.as_of`, `entity.history` and `map.snapshot` refuse a `computed`
    argument.
31. F12 (governance), `check_functions` works under the write lock — **Adjust**: artefact existence
    and module validation both move before the lock (rows are append-only, and a purge race leaves
    a state the design already handles); only pure change-set checks run inside it; pre-existing
    violations are warnings, so deletes and rollback still apply.
32. F13 (governance), the build order left steps 4-9 dishonest — **Accept**: the fence moves into
    step 4 with the first shapes older builds cannot read, and step 4 adds a "not implemented in
    this build" plan check that step 10 removes.
33. F1 (compat-perf), tail-call dispatch crashes debug builds — **Accept**: shared with SEC-2, plus
    the dev-profile `opt-level = 3` override for the five interpreter crates, since debug fuel costs
    about 90 times release, and a debug test of at least 200M fuel on a 2 MiB thread.
34. F2 (compat-perf), instantiation is not metered — **Accept**: initial memory is capped at 2 MiB
    by a module rule, `initial_memory_bytes` and `data_bytes` go in `ModuleInfo`, instantiation is
    pre-charged at 64 bytes per fuel unit, and the guest template sets a 64 KiB stack, checked in CI.
35. F3 (compat-perf), the caps bound output, not work — **Adjust**: adopted with SEC-6 (the scan
    cap, its fuel charge, the revision reuse promoted into the design, a digest memo for computed
    reads, the 100k-degree benchmark), except that the refusal text is fixed and `scanned` is not
    returned to callers or by `function.input`, because a count that includes hidden edges reveals
    how much is hidden; it goes to stats and traces for observers.
36. F4 (compat-perf), the output-cache total is unfair to late tenants — **Accept**: fair-share
    eviction as in SEC-16, with the 40-tenant test.
37. F5 (compat-perf), a cache hit hides a missing artefact — **Accept**: shared with SEC-10, and
    the two-instance PostgreSQL test covers it.
38. F6 (compat-perf), default function memory near half the pod — **Accept**: the defaults drop to
    `module_cache_mb = 64` and `output_cache_total_mb = 128`; the startup sum warns against the
    cgroup limit; the Helm values get a sizing comment; and `system.stats` reports function memory.
39. F7 (compat-perf), guard counter coverage — **Adjust**: the same resolution as SEC-12.
40. F8 (compat-perf), the base moved past PR #8 — **Accept**: verified (HEAD is an ancestor of
    origin/main, 30 commits behind, and migrations still end at 0014); step 0 rebases and re-checks
    the references and the lock, and the header says so.
41. F9 (compat-perf), two planned tests prove nothing — **Accept**: the sliced test runs with
    `slice_fuel` 1,000 and asserts a resume above the slice, and the lock test becomes structural
    (the counter, and a debug maximum hold time against a guest-free baseline), with latency
    targets only in onto-bench.
42. F10 (compat-perf), panics cannot become `function_error` — **Accept**: confirmed
    (`panic-strategy: abort`); `oi-fn/1` gains an optional `oi_panic` export, read after an
    `unreachable` trap within a 100k-fuel budget; G.5 is corrected; and the export is in A.3 and
    the tests.

Totals: 31 accepted, 11 adjusted, 0 rejected.
