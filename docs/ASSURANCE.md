# Assurance

The assurance layer answers three questions about an ontology and the graph it
describes:

- **What follows from it?** The reasoner derives facts from the graph and the
  schema's rules, keeps them apart from the stored facts, and writes a
  certificate that an independent checker accepts or refuses line by line.
- **What would a change do?** A schema plan lists not only the entities and
  links a change touches, but the derived facts it adds and removes, whether
  it is conservative over the vocabulary you already have, a risk word, the
  locks it breaks, the deletes that look like renames and the shape results it
  makes appear or disappear.
- **How does the data measure up?** Shapes validate the graph, lint reviews
  the schema, alignment compares two schemas, and induction reads a sheet into
  an object type with the evidence behind every line.

Every example on this page is a real run: the demo workspace (`onto init`,
`onto seed demo`) on PostgreSQL, with the manufacturing pack installed (see
[PACKS.md](PACKS.md)). Outputs are verbatim except where a `…` stands for
lines left out or for the rest of a long line.

## Where the ideas come from

The philosophy is taken from
[open-ontologies](https://github.com/fabio-rovai/open-ontologies) by Fabio
Rovai / Tesseract Semantics (MIT licence), described in *Open Ontologies:
Tool-Augmented Ontology Engineering with Stable Matching Alignment* (Rovai,
2026, [arXiv:2605.09184](https://arxiv.org/abs/2605.09184)). open-ontologies
works on OWL and RDF and checks its certificates against proofs written in
Lean.

onto-intelli reimplements the ideas for its own typed property graph (object
types, link types with inverses, interfaces, states and typed properties). It
shares no code with open-ontologies: the fact model, the rule table, the
certificate format (`oi-cert/1`) and the checker were written from a
specification for this engine. The checker, `onto-cert`, is a Rust crate with
tests; it is not a mechanized proof. See [Limitations](#limitations).

## Principles

| | Principle | What it means here |
|---|---|---|
| 1 | An inference is not an assertion | Derived facts are never written to the stored graph. They live in a closure object, are labelled `derived`, and carry the rule and premises that produced them. |
| 2 | An inference carries a certificate | Every derived fact has one recorded derivation: rule, conclusion, premises in the rule's body order, each premise asserted or concluded by an earlier line. A checker re-derives each line by pattern alone, without search, and refuses a forged line by its number and rule. |
| 3 | A rule is data, and an assumption is not a fact | Kind inheritance, interfaces and transitive and symmetric link types are a fixed built-in table compiled into the checker. Schema `[[rules]]` are supplied rules: a certificate that cites one earns `entailed_under_supplied_rules`, never `entailed`. |
| 4 | Checker words are the checker's | The engine says `derived`. Only a report produced by the `onto-cert` check function carries `entailed`, `entailed_under_supplied_rules` or `checked`; its type has a private constructor and cannot be deserialized. |
| 5 | Refuse rather than lie | A term that cannot be spelled safely means no certificate. The report says what was refused and why. |
| 6 | A partial run says so | `fixpoint_reached = false` means the closure is a lower bound, and every report built on it carries the flag. |
| 7 | A clean run is not a consistency result | Reports list the schema constructs the reasoner did not evaluate, so silence is never read as "valid". |
| 8 | Measured, proved and opinion are different words | A shape or lint report is a measurement. A checked certificate is a proof under its rule table. An alignment score is an opinion. Every report says which in its `means` field. |
| 9 | Plan before apply, semantically | A schema plan shows the derived facts that appear and disappear, conservativity, risk and lock violations, not only the blast radius. |
| 10 | Determinism | Every traversal is sorted before output: the same graph, schema and scope give the same bytes. |

## The fact model

The reasoner reads the graph as triples `(subject, predicate, object)` of
terms. Every term has one canonical spelling, which is also how certificates
write it:

| Term | Spelling | Example |
|---|---|---|
| entity | `<` + id + `>` | `<issue:421>` |
| schema name: object type, interface, derived class | `#` + name | `#issue` |
| relation (link type or derived relation), as predicate or as the subject of a characteristic | bare name | `depends_on` |
| built-in keyword | bare keyword | `a`, `extends`, `implements`, `state`, `transitive`, `symmetric` |
| property predicate | `prop.` + name | `prop.priority` |
| literal value | compact JSON | `"high"`, `42`, `true` |
| rule variable (rules only) | `?` + name | `?x` |

The asserted facts it reads, for the entities the caller may see (and, with a
workspace, only that workspace's entities; schema facts always):

| Fact | When |
|---|---|
| `<id> a #kind` | every visible entity, with its stored kind |
| `#child extends #parent` | every object type with `extends` |
| `#kind implements #iface` | every `implements` entry |
| `rel a transitive` | every link type with `transitive = true` |
| `rel a symmetric` | every link type with `symmetric = true` |
| `<from> rel <to>` | every stored link whose two endpoints are visible, in canonical direction |
| `<id> state "s"` | only when some rule body uses `state` or a variable predicate |
| `<id> prop.p <json>` | only for properties some rule body names; null or absent values are skipped |

The reserved keywords are `a extends implements state transitive symmetric`. A
link type or a derived relation named like one cannot be reasoned over: the
reasoner refuses it and lint reports it. A term containing a TAB, LF or CR, an
entity id containing `>`, or an empty name is refused too. Refused terms are
counted and listed (up to 20); the closure still computes over the remaining
facts, but no certificate is written for it.

## Built-in and supplied rules

The built-in table is fixed and identical in the engine and the checker.
`onto cert table` prints the checker's copy:

```text
$ onto cert table
builtin/type_extends	?x	a	?p	?x	a	?k	?k	extends	?p
builtin/extends_transitive	?k	extends	?q	?k	extends	?p	?p	extends	?q
builtin/type_implements	?x	a	?i	?x	a	?k	?k	implements	?i
builtin/transitive	?x	?r	?z	?r	a	transitive	?x	?r	?y	?y	?r	?z
builtin/symmetric	?y	?r	?x	?r	a	symmetric	?x	?r	?y
```

Each line is `name, head (s p o), body atoms (s p o)…`. The `builtin/` prefix
is reserved: the checker rejects a rule named `builtin/…` whose line differs
from its own.

Schema `[[rules]]` are **supplied rules**, named `schema/<name>` in a
certificate. Two forms compile to the same Horn rules:

```toml
# Chain form: follow these relations in order, infer one.
[[rules]]
name = "responsible_team"
chain = ["assigned_to", "member_of"]
infer = "handled_by_team"

# when/then form: a body of atoms and one head.
[[rules]]
name = "suspect_lot"
description = "A lot made by an order that consumed a quarantined input lot is suspect until cleared."
when = [["?lot", "lot_from_order", "?o"], ["?o", "consumed_lot", "?input"], ["?input", "state", "quarantined"]]
then = ["?lot", "a", "suspect_lot"]
```

They compile to these `rules.tsv` lines (from a real certificate):

```text
schema/responsible_team	?x0	handled_by_team	?x2	?x0	assigned_to	?x1	?x1	member_of	?x2
schema/suspect_lot	?lot	a	#suspect_lot	?lot	lot_from_order	?o	?o	consumed_lot	?input	?input	state	"quarantined"
```

A chain step written as an inverse name (`owned_by`) or `^rel` becomes the
canonical relation with subject and object swapped. The `when`/`then` syntax,
what each position accepts and the compile errors are in
[SCHEMA.md](SCHEMA.md#rules).

The two verdict words follow from which rules a certificate cites:

| Verdict | When | Means |
|---|---|---|
| `entailed` | every cited rule is a built-in line | the derived facts follow from the asserted facts under the schema's own semantics |
| `entailed_under_supplied_rules` | at least one cited rule is a schema rule | they follow **if** the supplied rules hold; the rules are assumptions the certificate carries, not facts it establishes |

## Running the reasoner

`onto reason run` (action `reason.run`) computes the closure over what the
caller may read: semi-naive forward chaining in rounds, every round's new
conclusions sorted and appended at its end, so every premise comes from an
earlier line.

```text
$ onto reason run
62 facts derived from 345 asserted (16 rules · 3 rounds · fixpoint reached)
  builtin/transitive                       3
  builtin/type_implements                  19
  schema/accountable_team                  4
  schema/at_risk_production_order          1
  schema/bom_explosion                     3
  schema/bom_explosion_multilevel          1
  schema/critical_quality_exposure         1
  schema/escalation                        4
  schema/owning_team                       12
  schema/production_crew                   3
  schema/responsible_team                  10
  schema/suspect_lot                       1
not evaluated: cardinality, required_properties, state_values, required_links, link_endpoints, property_types, shapes, metrics, derived_properties, quality_constraints, action_preconditions
  a clean run is not a consistency result
Sample derivations (derived, not asserted)
  <artifact:postgres-ha-tf> a #knowledge_item  by builtin/type_implements from <artifact:postgres-ha-tf> a #artifact, #artifact implements #knowledge_item
  …
  <issue:421> escalate_to <person:sarah>  by schema/escalation from <issue:421> affects <service:payment-api>, <team:payments> owns <service:payment-api>, <person:sarah> manages <team:payments>
  …
```

The `not evaluated` line is part of every run: the reasoner derives facts, it
does not check cardinality, required properties, state values, required
links, link endpoints, property types, shapes, metrics, derived properties,
quality constraints or action preconditions. Shapes and the quality report
measure some of those.

Limits stop a run early and say so (`fixpoint_reached: false`, `truncated`
with the reason): `max_derived` (default 1,000,000 derived facts; it stops a
round inside the rule join, not only between rounds), `max_rounds` (default
10,000) and `max_work` (default 50,000,000 candidate facts tried by the rule
joins of one run). A caller may lower `max_derived`, never raise it, and
`reason.run` lists at most 1,000 sample derivations:

```text
$ onto reason run --max-derived 10 --samples 0
10 facts derived from 345 asserted (16 rules · 1 round · fixpoint NOT reached: a lower bound)
  truncated: stopped at max_derived = 10
  builtin/type_implements                  10
not evaluated: cardinality, required_properties, state_values, required_links, link_endpoints, property_types, shapes, metrics, derived_properties, quality_constraints, action_preconditions
  a clean run is not a consistency result
means: Derived facts are the engine's opinion under the rule table: …
```

The facts are collected under the graph's read guard and the closure runs
after it is released. The engine keeps the last four closures, keyed by graph
revision, schema version, principal, roles, workspaces and workspace; a
closure of an older revision or schema version leaves the cache.

`onto reason query` matches a pattern over asserted and derived facts, with
the same atom syntax as `when`:

```text
$ onto reason query '[["?lot","a","suspect_lot"],["?lot","lot_of_part","?p"]]'
?lot                           ?p                             
 <production_lot:l-2420>        <part:hb-100>                 
1 binding
```

A query has at most 8 atoms and returns at most `limit` bindings (default 50,
at most 1,000). Its join may try 10,000,000 candidate facts and scan 100,000
matches; an answer cut there says so in `truncated`. State and property facts
are in the closure only when a rule reads them, so an atom that asks for
others is refused, and a variable predicate is answered without them:

```text
$ onto reason query --limit 3 '["issue:421","?p","?o"]'
?o                             ?p                             
 "blocked"                      state                         
 #issue                         a                             
 #work_item                     a                             
3 bindings · truncated: atom 0 has a variable predicate: the closure holds state and property facts only when a rule reads them (state facts are held; property facts are held only for prop.severity), so bindings for the rest are left out; read property values from the store
$ onto reason query '["?i","prop.priority","critical"]'
error: invalid: atom 0: the closure holds no prop.priority facts because no rule reads them; read property values from the store
$ onto reason query '[["?a","a","issue"],["?a","assigned_to","?b"],["?b","member_of","?c"],["?c","owns","?d"],["?d","a","project"],["?a","belongs_to","?d"],["?b","a","person"],["?c","a","team"],["?d","owned_by","?c"]]'
error: invalid: a query has at most 8 atoms (this one has 9)
```

When the closure refused terms (see [The fact model](#the-fact-model)), the
answer carries `refused`, their count, and says in `truncated` that the facts
naming them are left out.

## Certificates

`onto reason certify --out <dir>` (action `reason.certify`) writes four files
and checks them again, in the same process, with the `onto-cert` library:

| File | Content |
|---|---|
| `rules.tsv` | the built-in lines first, then the supplied rules in schema order |
| `asserted.tsv` | `s<TAB>p<TAB>o`, sorted, unique |
| `derivations.tsv` | `rule<TAB>conclusion (3 terms)<TAB>premises (3 terms each)`, premises in the rule's body order, in certificate order |
| `manifest.json` | format `oi-cert/1`, the three SHA-256 digests and counts (`derived`), and the engine's own claims (`attested`) |

```text
$ onto reason certify --out cert
entailed_under_supplied_rules 345 asserted · 62 derivations · rules cited: 2 built-in, 10 supplied · onto-cert 0.2.0
means: every derived fact follows from asserted.tsv under the cited rules, re-derived line by line by pattern without search; the supplied (non-built-in) rules are assumptions the certificate carries, not facts it establishes; this does not show that asserted.tsv is the graph the engine read
wrote cert (check again with `onto cert check cert` or `onto-cert check cert`)
```

```json
{
  "format": "oi-cert/1",
  "derived": {
    "asserted_sha256": "25a294513c83da9a5a57189d78bf29adaeafc7e9b6d8e34e6224ab0f05a1c342",
    "rules_sha256": "a3ef14701493ad66e8d731146246bc25612371c1f40163e2fdd05fece8ca154b",
    "derivations_sha256": "d787e92ccc1478e13008b9cf2cac6280f3260aa801e78ca8d9f527b0c9ff4926",
    "asserted": 345,
    "rules": 16,
    "derivations": 62
  },
  "attested": {
    "engine": "onto-intelli 0.2.0",
    "created_at": "2026-10-24T13:33:11Z",
    "workspace": null,
    "schema_version": 2,
    "graph_revision": 1,
    "principal": "local",
    "fixpoint_reached": true,
    "rounds": 3
  },
  "means": "derived fields are recomputed by the checker; attested fields are claims of the engine that wrote this certificate"
}
```

The checker never reads the attested fields, so whether the closure reached
its fixpoint stays the engine's claim. `reason.certify` returns it next to
the verdict, as `fixpoint_reached` and `rounds` (and `truncated` when a limit
cut the run). For a closure that stopped at a limit, `onto reason certify`
prints a note under the verdict that the closure is a lower bound and why it
stopped, and the web UI shows the same: every listed derivation still checks,
but more may follow.

### The checker: onto-cert

`crates/onto-cert` is a library and a binary that depend on the standard
library, `sha2` and `serde_json` only. A test fails the build if its
`Cargo.toml` names any `onto-` crate: a checker that shares code with the
engine is not independent of it. It parses the files from the format
specification, and checks, in order:

1. **Parse.** Wrong field counts, empty terms, a rule whose head variable is
   not in its body, or duplicate rule names: exit 2, `malformed`, with file,
   line and reason.
2. **Built-in table.** A `builtin/…` rule must match the compiled-in line:
   otherwise `forged_builtin_rule`.
3. **Manifest.** When present, the three digests and counts are recomputed:
   any mismatch is `digest_mismatch` with the field.
4. **Every derivation line**, in order: the rule exists (`unknown_rule`), the
   premise count equals the body length (`arity`), body atom *j* unifies with
   premise *j* under one binding (`premise_mismatch`), the instantiated head
   equals the conclusion (`conclusion_mismatch`), and every premise is
   asserted or the conclusion of an earlier line (`premise_not_known`). The
   first failure stops the check: exit 1.
5. **Success**: exit 0 with the verdict word.

Both `onto cert check <dir>` and the standalone `onto-cert check <dir>` run
offline: no store, no server, no configuration.

```text
$ onto cert check cert
{
  "asserted": 345,
  "checker": "onto-cert 0.2.0",
  "derivations": 62,
  "means": "every derived fact follows from asserted.tsv under the cited rules, re-derived line by line by pattern without search; the supplied (non-built-in) rules are assumptions the certificate carries, not facts it establishes; this does not show that asserted.tsv is the graph the engine read",
  "ok": true,
  "rules_cited": {
    "builtin": 2,
    "supplied": 10
  },
  "rules_sha256": "a3ef14701493ad66e8d731146246bc25612371c1f40163e2fdd05fece8ca154b",
  "verdict": "entailed_under_supplied_rules"
}
```

Forge one conclusion (line 15 says issue 421 is handled by Platform instead
of Payments) and recompute the manifest digest so the files are
self-consistent:

```text
$ diff cert/derivations.tsv forged/derivations.tsv
15c15
< schema/responsible_team	<issue:421>	handled_by_team	<team:payments>	<issue:421>	assigned_to	<person:kenji>	<person:kenji>	member_of	<team:payments>
---
> schema/responsible_team	<issue:421>	handled_by_team	<team:platform>	<issue:421>	assigned_to	<person:kenji>	<person:kenji>	member_of	<team:payments>
$ onto cert check forged
{
  "asserted": 345,
  "conclusion": "<issue:421> handled_by_team <team:platform>",
  "derivations": 62,
  "first_rejected": 15,
  "ok": false,
  "premises": [
    "<issue:421> assigned_to <person:kenji>",
    "<person:kenji> member_of <team:payments>"
  ],
  "reason": "conclusion_mismatch",
  "rule": "schema/responsible_team"
}
```

The exit code is 1. Without the recomputed digest the check stops earlier:

```text
$ onto cert check nodigest
{
  "actual": "dc955e479a6c70d142354a60ebaaf542365d055c9e0ffa355f612138069a04d1",
  "asserted": 345,
  "derivations": 62,
  "expected": "d787e92ccc1478e13008b9cf2cac6280f3260aa801e78ca8d9f527b0c9ff4926",
  "field": "derivations_sha256",
  "ok": false,
  "reason": "digest_mismatch"
}
```

And a rule table that cannot be parsed exits 2:

```text
$ onto-cert check malformed
{
  "error": "malformed",
  "file": "rules.tsv",
  "line": 6,
  "ok": false,
  "reason": "head variable `?x9` does not occur in the body"
}
```

| Exit | `onto cert check`, `onto-cert check` | `onto reason certify` |
|---|---|---|
| 0 | accepted | written and accepted |
| 1 | rejected | the in-process check rejected it |
| 2 | malformed or unreadable | malformed |
| 4 | | the command failed before a verdict (denied, unreachable server) |

The action `reason.check_certificate` runs the same library on files sent as
text, without graph access.

### Binding

A digest binds a certificate to bytes, not to a state of the world.
`onto reason binding <dir>` (action `reason.binding`) recomputes the asserted
facts under the same scope now and compares them with `asserted.tsv`:

```text
$ onto reason binding cert
same_graph certificate 345 facts · store 345 · shared 345 · only in certificate 0 · only in store 0
  manifest asserted_sha256 matches the file
means: A digest binds a certificate to bytes, not to a state of the world. same_graph means the store, read now under this scope, yields exactly the certificate's asserted facts; it says nothing about the store when the certificate was written, nor about facts outside the scope.
```

A store that has changed reports `different_graph`, with up to 20 sample facts
on each side.

## Explanations, provenance, verification and slices

These read a closure and write nothing.

**Justifications** (`onto reason explain`, action `reason.explain`) are the
minimal sets of asserted facts a fact is derived from. Schema facts are
asserted facts and appear in them:

```text
$ onto reason explain issue:421 a work_item
<issue:421> a #work_item derived
  #1 2 asserted facts · rules builtin/type_implements · sufficient, minimal
      #issue implements #work_item
      <issue:421> a #issue
      ⊢ <issue:421> a #work_item  by builtin/type_implements from <issue:421> a #issue, #issue implements #work_item
```

```text
$ onto reason explain part:hb-100 uses_component part:al-6061
<part:hb-100> uses_component <part:al-6061> derived
  #1 4 asserted facts · rules schema/bom_explosion, schema/bom_explosion_multilevel · sufficient, minimal
      <bill_of_material:hb-100-a> bom_component <part:hb-120>
      <bill_of_material:hb-100-a> bom_for <part:hb-100>
      <bill_of_material:hb-120-b> bom_component <part:al-6061>
      <bill_of_material:hb-120-b> bom_for <part:hb-120>
      ⊢ <part:hb-100> uses_component <part:hb-120>  by schema/bom_explosion from <bill_of_material:hb-100-a> bom_for <part:hb-100>, <bill_of_material:hb-100-a> bom_component <part:hb-120>
      ⊢ <part:hb-120> uses_component <part:al-6061>  by schema/bom_explosion from <bill_of_material:hb-120-b> bom_for <part:hb-120>, <bill_of_material:hb-120-b> bom_component <part:al-6061>
      ⊢ <part:hb-100> uses_component <part:al-6061>  by schema/bom_explosion_multilevel from <part:hb-100> uses_component <part:hb-120>, <part:hb-120> uses_component <part:al-6061>
```

Three words, three claims:

| Word | Claim |
|---|---|
| `sufficient` | an independent evaluator re-derives the fact from exactly these facts |
| `minimal` | it no longer does once any one of them is removed (each removal re-derived) |
| `complete` | every minimal justification was enumerated; claimed only when no bound stopped the search (default 10 justifications, at most 100 on request, and 10,000 steps) and the closure reached its fixpoint |

One justification comes from deletion-based minimization of a witness, more
from Reiter's hitting-set tree.

**Provenance** (`--semiring why|trust|count`, action `reason.provenance`)
evaluates a fact's support in a semiring. `trust` is the best derivation's
weakest premise, weighted by each entity's `confidence` property when it lies
in [0, 1] (schema facts weigh 1.0): it propagates an opinion recorded in the
graph, it measures nothing. `count` is the number of derivation trees, a lower
bound flagged `exact: false` when the support has a cycle. `why` returns the
witness sets.

```text
$ onto reason explain part:hb-100 uses_component part:al-6061 --semiring count
<part:hb-100> uses_component <part:al-6061> derived semiring count
  value 1
exact
```

**Verification** (`onto reason verify`, action `reason.verify`) checks claims
written in schema vocabulary, such as an LLM's answer, against the graph and
the closure. A claim may carry the rule and premises of a reasoning step:

```json
[
  {"subject": "issue:421", "relation": "assigned_to", "object": "person:kenji"},
  {"subject": "issue:421", "relation": "handled_by_team", "object": "team:payments"},
  {"subject": "issue:421", "relation": "state", "object": "resolved"},
  {"subject": "issue:421", "relation": "handled_by_team", "object": "team:platform"},
  {"subject": "issue:421", "relation": "owned_by_vendor", "object": "vendor:V-100"},
  {"subject": "issue:421", "relation": "handled_by_team", "object": "team:platform",
   "rule": "responsible_team",
   "premises": [{"subject": "issue:421", "relation": "assigned_to", "object": "person:maria"},
                {"subject": "person:maria", "relation": "member_of", "object": "team:platform"}]}
]
```

```text
$ onto reason verify @claims.json
asserted             issue:421 assigned_to person:kenji  <issue:421> assigned_to <person:kenji> is asserted
derived              issue:421 handled_by_team team:payments  <issue:421> handled_by_team <team:payments> is derived in 1 step(s) from 2 asserted fact(s)
    ⊢ <issue:421> handled_by_team <team:payments>  by schema/responsible_team from <issue:421> assigned_to <person:kenji>, <person:kenji> member_of <team:payments>
contradicted         issue:421 state resolved  the stored value differs: <issue:421> state "blocked"
    conflicts with <issue:421> state "blocked"
not_entailed         issue:421 handled_by_team team:platform  <issue:421> handled_by_team <team:platform> is neither asserted nor derived
outside_vocabulary   issue:421 owned_by_vendor vendor:V-100  not in this schema or not visible: relation owned_by_vendor
not_entailed         issue:421 handled_by_team team:platform  <issue:421> handled_by_team <team:platform> is neither asserted nor derived
    step premises_not_in_graph schema/responsible_team applies, but 1 premise(s) are neither asserted nor derived in this closure
1 asserted · 1 contradicted · 1 derived · 2 not_entailed · 1 outside_vocabulary
```

| Verdict | Means |
|---|---|
| `asserted` | stored as given |
| `derived` | in the closure, with its derivation down to asserted leaves |
| `contradicted` | a stored single-valued fact says otherwise: a state, a property value, a cardinality-one link, an entity's kind |
| `not_entailed` | absent from this closure under its rules; absence, not falsity |
| `outside_vocabulary` | the claim uses a relation, class, property or entity this schema and this reader do not have |

A claim that names a stored entity whose id the reasoner refuses to spell is
`outside_vocabulary` too, with a reason that says it cannot be reasoned over
and why; the report then counts the closure's `refused` terms.

A step label tells an invented premise from a misapplied rule:
`sound_step`, `premises_not_in_graph`, `rule_not_in_table`,
`rule_does_not_conclude_this` or `no_rule_concludes_this_relation`. It is the
engine's check, not a certificate verdict.

**Slices** (`onto reason slice`, action `reason.slice_check`) ask whether a
subset of entities keeps what the whole visible graph entails: the closure of
the facts whose entities all lie in the slice (plus schema facts) against the
full closure. This run came after the vendor sheet of
[Sheet induction](#sheet-induction-and-ingest) was ingested, hence 381
asserted facts rather than 345:

```text
$ onto reason slice issue:421 person:kenji team:payments service:payment-api --goals '[{"subject":"issue:421","relation":"handled_by_team","object":"team:payments"},{"subject":"issue:421","relation":"escalate_to","object":"person:sarah"}]'
slice of 4 entities: 1 preserved, 1 lost (full graph 381 asserted, 62 derived · slice 22 asserted, 3 derived)
  preserved          issue:421 handled_by_team team:payments
  lost               issue:421 escalate_to person:sarah
      missing <person:sarah> manages <team:payments>
means: Coverage is a proxy; preservation is the property. …
```

`lost` lists the asserted leaves of the full derivation the slice lacks.
`slice_only` (the slice entails what the whole does not) is impossible for
these monotone rules and is reported loudly if it ever appears. Without goals,
the report counts every derived fact over slice entities that the slice loses
and lists up to `limit` of them (default 100, at most 1,000).

## Semantic plans

Every `schema.plan` (CLI `onto schema plan`, also `pack.plan`) adds these
fields to the impact report described in [SCHEMA.md](SCHEMA.md#impact-and-breaking-changes):

| Field | What it holds |
|---|---|
| `consequences` | facts the closure holds under the planned schema and not the current one (`added`), the reverse (`removed`), per rule, with samples, rule and premises; `conservative` and `verdict` |
| `risk`, `risk_reasons` | `low`, `medium` or `high`, each reason spelled out |
| `locked_violations` | locked definitions the change deletes, renames or breaks |
| `likely_renames` | deletes paired with similar new definitions |
| `shapes` | shape results the planned schema adds (`new_results`) and resolves, with the verdict before and after |

Both closures are computed over the same current graph, as the planner may see
it: the facts for both are collected under the graph's read guard and the
closures run after it is released, and the closure of the active schema is
taken from the reasoner's cache when one matches. Migrations are not applied
to them; their data edits are counted under `migration`. Consequences and the
shape delta are skipped (with a reason) when the asserted facts exceed
500,000. A filed proposal keeps the plan just made instead of planning twice.

### Consequences and conservativity

Marking `depends_on` transitive changes no entity and no link. It still
changes what the ontology says:

```toml
message = "dependencies chain: if A needs B and B needs C, A needs C"

[[changes]]
op = "put_link_type"
name = "depends_on"
[changes.def]
from = ["project", "task", "service", "issue", "milestone", "package"]
to = ["project", "task", "service", "issue", "milestone", "repository", "environment", "package"]
inverse = "required_by"
cardinality = "many"
dependency = true
transitive = true
weight = 0.9
description = "The subject needs the object to work or to finish."
```

```text
$ onto schema plan transitive.toml
non-breaking on schema v2 · 0 entities · 0 links in scope
  · change link type project|task|service|issue|milestone|package -depends_on-> project|task|service|issue|milestone|repository|environment|package (now transitive) (0 entities, 0 links)
  risk medium: not conservative: 4 added facts are stated purely in the current vocabulary, e.g. <project:alpha> depends_on <service:auth-service>
  consequences: +4 added (1 asserted, 3 derived), -0 removed · not conservative (4 new over existing names)
      asserted 1 · builtin/transitive 3
      + <project:alpha> depends_on <service:auth-service>  by builtin/transitive from depends_on a transitive, <project:alpha> depends_on <service:payment-api>, <service:payment-api> depends_on <service:auth-service>
      + <project:alpha> depends_on <service:postgres>  by builtin/transitive from depends_on a transitive, <project:alpha> depends_on <service:payment-api>, <service:payment-api> depends_on <service:postgres>
      + <task:34> depends_on <task:31>  by builtin/transitive from depends_on a transitive, <task:34> depends_on <task:32>, <task:32> depends_on <task:31>
      + depends_on a transitive
  shapes: 0 new results, 0 resolved · violates → violates
```

Zero entities affected, yet four new facts, all stated in names that already
existed: the change is **not conservative**. One of them is the new schema
fact itself (`depends_on a transitive`, asserted, counted under `asserted`);
the other three are derived by `builtin/transitive`. The consequences line
says which: `+N derived` when every added fact is the reasoner's, `+N
asserted` when every one is a schema fact the change states (a new `extends`
or `implements`, a link type made transitive), and `+N added (A asserted, D
derived)` for a mix.

The base signature is the current schema's names (object types, interfaces,
link types, rule-derived relations and classes) plus every entity, keyword and
literal. `new_over_existing_vocabulary` counts added facts whose terms are all
in it; `conservative` is true when that count is 0, both runs reached their
fixpoint and neither refused a term. `verdict` is `conservative`,
`not_conservative` or `undetermined` (skipped, a run stopped early, or terms
were refused: `refused` counts them, the facts naming them are left out of
both closures and the consequences line says so). A new rule that derives a
new relation name is conservative: everything it adds uses a name the current
vocabulary does not have.

### Risk

| Risk | When |
|---|---|
| `high` | breaking, a migration still needed, a lock violated without override, or the change set does not validate |
| `medium` | any delete or rename, any removed derived fact, or not conservative |
| `low` | purely additive and conservative |

The risk word is the planner's opinion; `risk_reasons` says why.

### Locks

An admin locks a definition so that no live change deletes, renames or breaks
it:

```text
$ onto lock object_types vendor --reason 'procurement reports read vendor ids'
object_types vendor locked by local at 2026-10-24T13:35:18Z: procurement reports read vendor ids
$ onto schema plan drop-vendor.toml
locked: apply refused on schema v3 · 15 entities · 21 links in scope
  · delete shape vendor (0 entities, 0 links)
  ! delete link type account_manager (0 entities, 15 links)
      15 links use account_manager, which the new schema no longer defines
  ! delete link type parent (0 entities, 6 links)
      6 links use parent, which the new schema no longer defines
  ! delete object type vendor (15 entities, 27 links)
      15 vendor entities have a kind the new schema no longer defines
  migration: 0 updated, 0 retyped, 15 deleted entities; 21 links deleted, 0 moved
  risk high: breaking: delete_link_type account_manager (15 links use account_manager, which the new schema no longer defines); breaking: delete_link_type parent (6 links use parent, which the new schema no longer defines); breaking: delete_object_type vendor (15 vendor entities have a kind the new schema no longer defines); touches locked object_types/vendor (delete_object_type vendor, locked by local) without override; removes or moves a definition: delete_shape vendor; removes or moves a definition: delete_link_type account_manager; removes or moves a definition: delete_link_type parent; removes or moves a definition: delete_object_type vendor
  consequences: +0 derived, -0 removed · conservative
  locked object_types vendor: deletes the locked definition (by local: procurement reports read vendor ids)
  shapes: 0 new results, 0 resolved · violates → violates
$ onto schema apply drop-vendor.toml
error: conflict: schema change touches locked definitions: object_types/vendor (delete_object_type vendor: deletes the locked definition; locked by local: procurement reports read vendor ids); an admin may pass override_locks
```

Sections: `object_types`, `link_types`, `interfaces`, `rules`, `metrics`,
`action_types`, `shapes` and `properties` (named `kind.prop`). Only admins
lock and unlock, never agents, and denials are audited. `override_locks` on a
change set is honoured for admins only; an overridden violation stays in the
plan marked `overridden` and in the `schema.change` audit event. Proposal
approval re-plans, so locks guard proposals too. Details in
[SCHEMA.md](SCHEMA.md#locks).

### Likely renames

A change set that deletes one definition and adds a similar one in the same
section is probably a rename that would lose data:

```text
$ onto schema plan rename-link.toml
non-breaking on schema v3 · 0 entities · 0 links in scope
  · delete link type in_repository (0 entities, 0 links)
  · add link type package -hosted_in_repository-> repository (0 entities, 0 links)
  warning in_repository looks renamed to hosted_in_repository (score 0.93): use rename_link_type to keep its 0 links
  risk medium: removes or moves a definition: delete_link_type in_repository
  consequences: +0 derived, -0 removed · conservative
  rename? link_types in_repository → hosted_in_repository score 0.93
      name similarity 0.76 (in_repository vs hosted_in_repository)
      endpoints: from 1.00, to 1.00 alike
      same cardinality
      dependency/transitive/symmetric: 3 of 3 flags equal
  shapes: 0 new results, 0 resolved · violates → violates
```

The score is 0.3 × name similarity + 0.7 × structural similarity (object
types: property names, extends and implements, states; link types: endpoints,
cardinality, flags; properties: type and values). Pairs scoring 0.6 or more
are reported, matched one to one.

### Shape delta

`shapes` compares the shape report under the current and the planned schema
on the current graph (asserted facts only): results the change creates,
results it resolves, and the verdict on each side. It is a measurement like
any shape report.

### Consequences of an action

`onto do <action> … --consequences` (action `action.plan` with
`consequences: true`) stages the action's edits on a fork and compares the
closures. The plan is not applied:

```text
$ onto do assign item=issue:433 assignee=person:maria --consequences
planned assign
  • link issue:433 -assigned_to-> person:maria (replacing person:kenji)
  • update issue:433: state open → triaged
  • record event work.assigned on issue:433
  consequences: +3 added (2 asserted, 1 derived), -3 removed · not conservative (3 new over existing names)
      asserted 2 · schema/responsible_team 1
      + <issue:433> assigned_to <person:maria>
      + <issue:433> handled_by_team <team:platform>  by schema/responsible_team from <issue:433> assigned_to <person:maria>, <person:maria> member_of <team:platform>
      + <issue:433> state "triaged"
      - <issue:433> assigned_to <person:kenji>
      - <issue:433> handled_by_team <team:payments>
      - <issue:433> state "open"
  risk low · reversible
```

The new link and state are asserted facts the action writes; the new team is
derived from them. Conservativity is computed by the same definition, but a
data edit almost always adds facts over existing names; the `means` field
says so.

## Shapes

Shapes are SHACL-style conditions on the objects of a target, declared in the
schema (`[shapes.<name>]`, live ops `put_shape` and `delete_shape`; syntax in
[SCHEMA.md](SCHEMA.md#shapes)).

| | |
|---|---|
| Targets | `target` (a kind with its subtypes, an interface, or with `inferred` a derived class), `target_ids`, `target_subjects_of`, `target_objects_of` |
| Paths | a link type (out), an inverse name or `^rel` (in), a sequence `a.b`, `prop.x`, `state`, `title`; with `inferred`, relation steps also follow derived relations |
| Components | `min_count`, `max_count`, `class`, `datatype`, `in`, `has_value`, `pattern`, `min_length`, `max_length`, `min_value`, `max_value`, `node` (nested conformance, depth 5), `closed` (with `ignored_properties`) |
| Severity | `violation` (default), `warning`, `info`, per shape or per property shape |

`onto shapes validate` (action `shapes.validate`) answers with one of three
words, never "valid". The manufacturing sample holds one deliberate
violation:

```text
$ onto shapes validate
violates 15 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation production_order_shape   min_count    production_order:po-1004 scheduled_on       Schedule the order on a line or work center.
means: A shape report is a measurement of the graph as this principal can read it (with inferred, of the reasoner's closure too), not a proof. …
```

| Verdict | Exit | When |
|---|---|---|
| `conforms` | 0 | every selected shape with a target selected at least one focus node, no constraint was skipped, none produced a result |
| `violates` | 1 | some constraint produced a result, of any severity (as in SHACL) |
| `undetermined` | 3 | no result, but a constraint was skipped, a shape's target selected nothing, or the closure is a lower bound |

A key the engine does not evaluate is reported as skipped, which makes a
report without results `undetermined` rather than `conforms`. An id in
`target_ids` that does not exist, or that the reader cannot see, is a
`target_missing` result: a selected-but-absent node is a violation, not an
empty target.

`onto shapes check <file>` (action `shapes.check`) is a structural dry run, so
an author (or an agent) can iterate until it says ok:

```text
$ onto shapes check shapes-draft.toml
issues 1 shape · 3 constraints checked
  missing_kind       issue_triage issue_triage/0 class persn is not an object type, interface or derived class
  missing_property   issue_triage issue_triage/2 issue objects have no property severity
  unknown_key        issue_triage issue_triage/2 qualified_value_shape is not a constraint this engine evaluates; it would be skipped and leave the report undetermined
```

Validation is bounded. A path has at most 16 steps (the schema refuses a
longer one, and `shapes check` reports it as `too_large`); a validation
carries at most 64 draft shapes with 1,024 property shapes between them; and
path evaluation may take 20,000,000 steps, after which the report lists the
results of the focus nodes evaluated so far and records a `budget` skip, so a
cut run is `violates` when those results exist and `undetermined` otherwise,
never `conforms`. A `pattern` is at most 1,024 bytes and compiles within a size
limit.

```text
$ onto shapes check long-path.toml
issues 1 shape · 1 constraint checked
  too_large          team_reach team_reach/0 path assigned_to.member_of.^member_of.member_of.^member_of.member_of.^member_of.member_of.^member_of.member_of.^member_of.member_of.^member_of.member_of.^member_of.member_of.^member_of has 17 steps, more than 16
means: A structural dry run: …
```

With `inferred`, validation reads the reasoner's cached closure when it was
computed at the current graph revision and schema version, and computes one
otherwise.

### Shape induction

`onto shapes induce <kind>` (action `shapes.induce`) reads a shape from the
objects of a kind, with the counts behind every constraint, and prints it as a
schema layer:

```text
$ onto shapes induce production_order
# production_order_shape from 4 entities of kind production_order
# production_order_shape/title min_count 4 of 4 production_orders have a title
…
# production_order_shape/makes_part min_count 4 of 4 production_orders have a makes_part link
# production_order_shape/makes_part max_count 4 of 4 production_orders have exactly one makes_part link and none has more
# production_order_shape/makes_part class all 2 objects reached by makes_part are parts
# production_order_shape/scheduled_on max_count 3 of 4 production_orders have exactly one scheduled_on link and none has more
# production_order_shape/scheduled_on class all 3 objects reached by scheduled_on implement plant_floor_asset
# note: shape production_order_shape exists: saving this change set replaces it

[shapes.production_order_shape]
description = "Induced from 4 production_orders: describes the data as it is, not what it should be."
target = "production_order"
…
```

The rules: `min_count = 1` only when every object has the path; `max_count =
1` when none has more than one and at least half have one; `class` when every
target has one kind (or one shared interface); `datatype` from the observed
values; `in` when there are 2 to 12 distinct values and at least 5 objects per
value; never `pattern`. The objects it came from satisfy it by construction, so
it constrains the next object, not the existing ones.

## Lint and feedback

`onto lint` (action `schema.lint`) runs fixed heuristics over the schema and,
given the graph, over how much of the schema is used:

| Check | Severity | Flags |
|---|---|---|
| `missing_description` | info | an object type, link type, rule, shape or action type without a description |
| `missing_inverse` | info | a link type that is not symmetric and has no inverse name |
| `unconstrained_link` | warning | no `from` or no `to` list |
| `orphan_type` | info | a type nothing names, extends or backs |
| `extends_cycle` | error | object types extending each other in a cycle |
| `dead_rule` | warning | a rule that can never fire: its variables need kinds no type satisfies at once |
| `recursive_rule` | info | a rule head reachable from its own body |
| `keyword_collision` | error (warning for an inverse name) | a link type or derived relation named like a reserved keyword |
| `transitive_cardinality_one` | warning | a transitive link type with cardinality one |
| `symmetric_endpoints` | warning | a symmetric link type whose `from` and `to` differ |
| `state_lists` | error | terminal or alert states not among the states |
| `empty_enum` | error | an enum with no values |
| `flat_hierarchy` | info | more than 8 direct subtypes |
| `shape_reference` | error | a shape naming an unknown target, relation, property, class or shape, or an invalid pattern |
| `unused_interface` | info | an interface nobody implements |
| `duplicate_label` | warning | two types sharing one label |
| `unknown_rule_class` | error | a rule `when` using `a <name>` for an unknown class |
| `unused_type` | info, needs a graph | no visible object of the type |
| `unused_link_type` | info, needs a graph | no visible link of the type |

```text
$ onto lint
warning 54ee4638bd54 keyword_collision link_type:implemented_by
        the inverse name implements of link type implemented_by is ambiguous: relation name 'implements' is a reserved keyword; in rule and query atoms `implements` means the inverse of implemented_by, so the reasoner's own `implements` facts cannot be asked for
        fix: rename it (reserved words: a, extends, implements, state, transitive, symmetric; names may not start with ?, <, #, ", ^ or prop.)
warning bae8404da247 unconstrained_link link_type:about
        link type about accepts any target kind
        fix: list the kinds it connects in from = [...] and to = [...] of [link_types.about]
…
info    3697ee08a206 recursive_rule rule:bom_explosion_multilevel
        rule bom_explosion_multilevel derives relation uses_component, which its own body uses again through rules bom_explosion_multilevel -> bom_explosion_multilevel
        fix: nothing, if the recursion is intended (the reasoner runs it to a fixpoint); otherwise break the cycle between these rules
…
0 errors, 7 warnings, 24 info · 0 suppressed (dismissed) · 19 checks ran
```

A finding id is the first 12 hex digits of `sha256(check + "\0" + target)`,
so it survives edits elsewhere. Feedback is kept per id (write access, audited
as `schema.lint_feedback`). A dismissed finding leaves the report by default
and is counted as suppressed; an accepted one stays, marked accepted. Nothing
is hidden silently:

```text
$ onto lint --dismiss 3697ee08a206 --note 'multi-level BOM explosion is meant to recurse'
dismissed 3697ee08a206 recursive_rule rule:bom_explosion_multilevel · by local
$ onto lint --check recursive_rule --check keyword_collision
warning 54ee4638bd54 keyword_collision link_type:implemented_by
        …
0 errors, 1 warnings, 0 info · 1 suppressed (dismissed) · 2 checks ran
$ onto lint --check recursive_rule --all
info    3697ee08a206 recursive_rule rule:bom_explosion_multilevel [dismissed]
        rule bom_explosion_multilevel derives relation uses_component, which its own body uses again through rules bom_explosion_multilevel -> bom_explosion_multilevel
        fix: nothing, if the recursion is intended (the reasoner runs it to a fixpoint); otherwise break the cycle between these rules
        note: multi-level BOM explosion is meant to recurse
0 errors, 0 warnings, 1 info · 0 suppressed (dismissed) · 1 checks ran
```

`onto lint` exits 1 when it shows an `error` finding.

## Alignment and SSSOM

`onto align` (action `schema.align`) compares another schema (a pack, or a
TOML file) with the schema in force and scores every candidate pair. Scores
are opinions.

| Pair | Signals (weights) |
|---|---|
| object types | name 0.35 (Jaro-Winkler, token Jaccard, 1.0 through a glossary), properties 0.25, link types 0.20, parents 0.10, states 0.10 |
| link types | name 0.5, endpoints 0.5 (through the accepted kind matches) |

A signal neither side has data for is left out and the weights renormalized.
A link end that cannot be compared (no kind on it is matched) counts as half
agreement, 0.50, when the other end is compared, so that agreeing on one
shared end (every link type to `person` agrees on `to`) does not outweigh the
names; two ends that both admit any kind stay out of the score. A name that
is the same word through a glossary is reported with both definitions and the
word they share. Matching is greedy and one to one; a competitor within 0.02
makes a pair `borderline`. `exact_match` needs a score of 0.85 and shared
properties or link types (Jaccard 0.5); `close_match` needs 0.6. When a kind's
best target is already matched to the kind's ancestor (below, `issue` to
`ticket`, the parent of `incident`), the kind gets `broad_match` to it; the
converse gives `narrow_match`. `accepted` needs 0.75, no tie, and structure or
the same word. The report names the source by its pack id (`onto align --pack
supply-chain` reports `supply-chain → enterprise-execution`), else by the
TOML file's `name`.

```text
$ onto align helpdesk.toml --sssom helpdesk.sssom.tsv
helpdesk → enterprise-execution 2 accepted, 3 borderline, 1 source and 73 target unmatched

Object types
  accepted   employee                 close_match    person                   score 0.75
      name: 'employee' and 'person' are the same word through the glossary word 'employee' (1.00)
      properties: 3 of 3 names shared (jaccard 1.00), types agree on 3 of 3 (1.00)
      links: 0 of 12 link types correspond by name (0.00)
      parents: not compared (neither extends or implements anything)
      states: not compared (neither declares states)
      close_match: score 0.750 >= 0.6
      accepted: score 0.750 >= 0.75 and no competitor within 0.02
  borderline incident                 broad_match    issue                    score 0.596
      …
      broad_match: issue is matched to ticket, an ancestor of incident, so issue is broader
      borderline: score 0.596 is below 0.75
  borderline ticket                   close_match    issue                    score 0.691
      …
      states: 5 of 6 shared (0.83)
      close_match: score 0.691 >= 0.6
      borderline: score 0.691 is below 0.75

Link types
  accepted   assignee                 close_match    assigned_to              score 0.75
      name: 'assignee' and 'assigned_to' are the same word through the glossary word 'assignee' (1.00)
      from: not compared (no kind of [ticket] is mapped; counts as 0.50)
      to: [employee] vs [person, agent, team] (0.50)
      close_match: score 0.750 >= 0.6
      accepted: score 0.750 >= 0.75 and no competitor within 0.02
  borderline in_squad                 close_match    manages                  score 0.62
      name: 'in_squad' vs 'manages' (similarity 0.49)
      from: [employee] vs [person] (1.00)
      to: not compared (no kind of [squad] is mapped; counts as 0.50)
      close_match: score 0.620 >= 0.6
      borderline: score 0.620 is below 0.75
…
wrote helpdesk.sssom.tsv
means: scores are opinions; certify a mapping before you rely on it
```

The report carries a [SSSOM](https://mapping-commons.github.io/sssom/) TSV
(`#curie_map` header; columns `subject_id`, `subject_label`, `predicate_id`,
`object_id`, `object_label`, `mapping_justification`, `confidence`,
`comment`) and a change set of glossary terms for the accepted matches whose
names differ.

```text
#curie_map:
#  skos: http://www.w3.org/2004/02/skos/core#
#  semapv: https://w3id.org/semapv/vocab/
#  src: https://onto-intelli.dev/schema/helpdesk/
#  tgt: https://onto-intelli.dev/schema/enterprise-execution/
#mapping_set_id: https://onto-intelli.dev/align/helpdesk/enterprise-execution
#license: https://w3id.org/sssom/license/unspecified
#mapping_tool: onto-intelli 0.2.0
subject_id	subject_label	predicate_id	object_id	object_label	mapping_justification	confidence	comment
src:class/employee	employee	skos:closeMatch	tgt:class/person	person	semapv:CompositeMatching	0.750	accepted; …
```

**Certification** (`--certify`, action `schema.align_certify`) compares a
claimed mapping with both hierarchies (extends chains and implemented
interfaces) through the mapping itself. It can keep, weaken or leave a claim
uncertified, never strengthen it:

```text
$ onto align helpdesk.toml --certify mappings.json
uncertified        work_unit exact_match work_item  nothing to compare: neither side has an ancestor
upheld             ticket exact_match issue  exact_match upheld: carried source ancestors [work_item] vs comparable target ancestors [work_item] are equal
downgraded         incident exact_match → broad_match issue  exact_match refuted: carried source ancestors [issue, work_item] vs comparable target ancestors [work_item]; the source sits deeper, so issue is broader
uncertified        employee exact_match person  nothing to compare: neither side has an ancestor
means: certification compares positions in the two hierarchies (extends chains and implemented interfaces) through the mapping itself, not meanings: it can keep, weaken or leave a claimed match uncertified, never strengthen it
```

`S skos:broadMatch T` means T is broader, as in SKOS.

## Sheet induction and ingest

`onto induce <file.csv>` (action `model.induce`) reads one sheet and proposes
the object type, links and shape that describe it, each with an evidence
sentence. The CSV reader handles RFC 4180 quoting, a BOM, and `,` `;` TAB or
`|` delimiters, up to 50 MB and 1,000,000 rows.

```text
$ onto induce vendors.csv --change-set vendors.change.json
vendor from 15 rows vendor: the singular of the file name "vendors"
  key    vendor_code vendor_code is filled and unique in all 15 rows; its name reads as a key: ids are vendor:<vendor_code>
  title  name name: its name reads as a title; filled in 15 of 15 rows
  …
  tier                     property  enum       required · 15 filled, 3 distinct · tier: property tier, enum: 3 distinct values (bronze, gold, silver) in 15 filled rows, at least 5 rows per value (filled in all 15 rows): required
  rating                   property  float      required · 15 filled, 15 distinct · rating: property rating, float: all 15 filled values are numbers, not all integers (filled in all 15 rows): required
  active                   property  bool       required · 15 filled, 2 distinct · active: property active, bool: all 15 filled values are true, false, yes or no (filled in all 15 rows): required
  onboarded                property  timestamp  required · 15 filled, 15 distinct · onboarded: property onboarded, timestamp: all 15 filled values are RFC 3339 timestamps or YYYY-MM-DD dates (filled in all 15 rows): required
  …
  link account_manager → person 15/15 cells match · account_manager: 15 of 15 filled values are keys of existing person objects in workspace acme (100%, at least 90%); a new link type account_manager (vendor → person, at most one per vendor: one cell per row), inverse vendors
  link parent → vendor 6/6 cells match · parent_code: all 6 filled values are keys of this sheet's own rows; a new link type parent (vendor → vendor, named after parent), inverse parent_of
  shape vendor/title min_count 1 on title: 15 of 15 rows fill name
  shape vendor/prop.tier prop.tier: min_count 1: tier is filled in all 15 rows; in: the 3 values tier shows
  …
wrote vendors.change.json (4 changes): `onto schema plan vendors.change.json`, then `onto schema apply vendors.change.json`, then `onto induce vendors.csv --ingest`
means: describes the data as it is, not what it should be: every type, key, link and shape constraint is a measurement of these rows, and the change set is a proposal to plan and review
```

Column types: int, float, bool (`true/false/yes/no`), timestamp (RFC 3339 or
`YYYY-MM-DD`), enum (2 to 12 distinct values and at least 5 rows per value),
text (average length above 80) or string. A column is required only when every
row fills it. A column whose values are at least 90% keys of existing objects
of one kind becomes a link type; one whose values are keys of the sheet's own
rows becomes a self link.

The change set goes through the normal plan and apply, then the rows are
ingested in one atomic apply (action `ingest.sheet`):

```text
$ onto schema apply vendors.change.json
…
applied schema v3
$ onto induce vendors.csv --ingest --dry-run
dry run vendor 15 rows · 15 objects · 21 links
$ onto induce vendors.csv --ingest
ingested vendor 15 rows · 15 objects · 21 links · revision 2
$ onto shapes validate --shape vendor
conforms 15 focus nodes · 0 violations, 0 warnings, 0 info
```

Ingest applies a lexical guard: a cell that does not read as its declared type
is left out and reported, never coerced; a link to a missing object is left
out and reported; a row missing a required value is left out:

```text
$ onto induce vendors-new.csv --kind vendor --ingest --dry-run
dry run vendor 2 rows · 0 objects · 0 links
  bad_value row 1 rating vendor.rating: not a number: value left out
  missing_required row 1 rating vendor.rating is required and this row has no readable value: row left out
  bad_value row 2 active vendor.active: not true, false, yes or no: value left out
  bad_value row 2 onboarded vendor.onboarded: not an RFC 3339 timestamp or a YYYY-MM-DD date: value left out
  missing_required row 2 active vendor.active is required and this row has no readable value: row left out
```

With `--kind`, columns are matched to the existing type by name. A column
named like one of the type's link types (as written, by its reference stem,
as `parent_code` names `parent`, or prefixed with the kind) is that link even
when this sheet's cells do not name existing objects often enough to read as
references, and each missing target is reported row by row. Here `lena` and
`V-999` do not exist:

```text
$ cat vendors-2026.csv
vendor_code,name,tier,rating,active,onboarded,account_manager,parent_code
V-120,Contoso Springs,gold,4.5,yes,2026-01-10,lena,V-999
V-121,Northwind Wire,silver,3.8,yes,2026-02-02,kenji,V-100
V-122,Fabrikam Plating,bronze,3.2,no,2026-02-17,maria,
$ onto induce vendors-2026.csv --kind vendor --ingest --dry-run
dry run vendor 3 rows · 3 objects · 3 links
  missing_target row 1 account_manager no object person:lena: account_manager link left out
  missing_target row 1 parent_code no object vendor:V-999: parent link left out
means: rows read with the mapping's declared types; …
```

A column that matches nothing the type declares stops the ingest. `--mapping
FILE` says where each column goes instead (JSON: `{kind, key, title?, state?,
properties: {column: property}, links: {column: {rel, target_kind}}}`):

```text
$ cat suppliers.csv
code,company,level,score,is_active,since,manager
V-130,Tailwind Bolts,gold,4.4,yes,2026-03-01,sarah
V-131,Graphic Gaskets,silver,3.5,yes,2026-03-15,john
$ onto induce suppliers.csv --kind vendor --ingest --dry-run
error: invalid: the sheet's rows are vendor objects, but these columns match nothing vendor declares: level (no property of vendor by that name), score (no property of vendor by that name), is_active (no property of vendor by that name), since (no property of vendor by that name); pass a mapping that says where they go (or leaves them out), or add them to vendor first
$ cat suppliers.mapping.json
{
  "kind": "vendor",
  "key": "code",
  "title": "company",
  "properties": {"level": "tier", "score": "rating", "is_active": "active", "since": "onboarded"},
  "links": {"manager": {"rel": "account_manager", "target_kind": "person"}}
}
$ onto induce suppliers.csv --ingest --mapping suppliers.mapping.json --dry-run
dry run vendor 2 rows · 2 objects · 2 links
means: rows read with the mapping's declared types; …
```

## Context preservation

When the orchestrator packs facts into a token budget for the LLM, a packed
`derived` fact is useless, or misleading, without the facts it rests on.
After packing, `context.build` checks each derived fact: are the links its
derivation walks spelled by packed facts (a relations line, a provenance line
or a premise fact)? If not and budget remains, compact `premise` facts are
appended (`repaired`); otherwise the claim is `lost`. The order and text of
the packed facts never change.

`onto call context.build '{"question":"Which team handles issue #421?"}'`
returned this `preservation` (its `means` left out here) with the default
budget:

```json
{
 "claims": 4,
 "lost": [],
 "preserved": 4,
 "repaired": 0
}
```

and with `"token_budget": 200`:

```json
{
 "claims": 4,
 "lost": [
  "issue:421 owning_team team:payments",
  "issue:421 accountable_team team:payments",
  "issue:421 handled_by_team team:payments",
  "issue:421 escalate_to person:sarah"
 ],
 "preserved": 0,
 "repaired": 0
}
```

At 200 tokens the four derived facts still fit (they outrank relations
lines), but the links they rest on do not, and no premise fits in what is
left. The field is omitted for questions whose context holds no derived fact,
so their JSON is unchanged. It is a measurement of the packed text.

## Surfaces

Every capability is a catalogue action, so the CLI, HTTP, gRPC, MCP and the
library reach it the same way, under the same permissions.

| Action | w | CLI | MCP tool |
|---|---|---|---|
| `reason.run` | | `onto reason run` | |
| `reason.query` | | `onto reason query` | `query_inferred` |
| `reason.explain` | | `onto reason explain` | `explain_fact` |
| `reason.provenance` | | `onto reason explain --semiring` | |
| `reason.certify` | | `onto reason certify --out` | `certify_reasoning` |
| `reason.check_certificate` | | (`onto cert check` runs the same library offline) | |
| `reason.binding` | | `onto reason binding` | |
| `reason.verify` | | `onto reason verify` | `verify_claims` |
| `reason.slice_check` | | `onto reason slice` | `check_slice` |
| `shapes.validate` | | `onto shapes validate` | `validate_shapes` |
| `shapes.induce` | | `onto shapes induce` | `induce_shape` |
| `shapes.check` | | `onto shapes check` | `check_shapes` |
| `schema.lint` | | `onto lint` | `lint_schema` |
| `schema.lint_feedback` | w | `onto lint --dismiss/--accept` | |
| `schema.locks` | | `onto locks` | |
| `schema.lock`, `schema.unlock` | w | `onto lock`, `onto unlock` | |
| `schema.align` | | `onto align` | `align_schema` |
| `schema.align_certify` | | `onto align --certify` | |
| `pack.list`, `pack.show`, `pack.plan` | | `onto pack list/show/plan` | `list_packs`, `show_pack`, `plan_pack` |
| `pack.install` | w | `onto pack install` | `install_pack` (always a proposal) |
| `model.induce` | | `onto induce` | `induce_sheet` |
| `ingest.sheet` | w | `onto induce --ingest` | |

`schema.plan` and `action.plan {consequences}` carry the plan fields above.
HTTP adds `POST /v1/reason/query`, `POST /v1/reason/certify`,
`POST /v1/reason/verify`, `POST /v1/shapes/validate`, `GET /v1/schema/lint`,
`GET /v1/packs`, `GET /v1/packs/{id}`, `POST /v1/packs/{id}/plan` and
`POST /v1/packs/{id}/install`; gRPC has an `AssuranceService` with one RPC per
action. No action takes a server-side path or URL: the CLI reads files and
sends their text. The web UI has an **Assurance** area ([UI.md](UI.md)).

Agents never lock or unlock, and `pack.install` from an agent always files a
proposal.

### MCP tools and profiles

`onto mcp` offers every tool by default. A profile narrows the list to one job,
so an agent sees fewer, more relevant tools. Precedence:
`onto mcp --profile <name>`, then `ONTO_MCP_PROFILE`, then `[mcp] profile` in
the config. Every profile includes `ask`, `search`, `get_entity` and
`get_schema`; write tools still need `[mcp] allow_writes = true`. The
`initialize` instructions name the active profile, what it withheld and how
many of the write tools it offers `allow_writes = false` hides, so the three
counts add up to every tool. Piping an `initialize` and a `tools/list` request
into `onto mcp --profile reasoning` (no write tools) and `onto mcp --profile
full` returned instructions that end with:

```text
Tool profile: reasoning (16 of 51 tools offered, 35 withheld by the profile; set [mcp] profile, ONTO_MCP_PROFILE or onto mcp --profile to change it).
Tool profile: full (48 of 51 tools offered, 0 withheld by the profile, 3 write tools hidden by allow_writes = false; set [mcp] profile, ONTO_MCP_PROFILE or onto mcp --profile to change it).
```

The tools `tools/list` returned under each profile in those runs (with
`allow_writes = false`):

| Profile | Tools |
|---|---|
| `full` (48) | `ask search get_entity neighbors find_data join_path trusted_metric authority learned_facts find_entities explain_blockers impact dependencies critical_path metric history changes experts data_quality what_if list_actions plan_action get_schema plan_schema_change propose_schema_change draft_ontology refine_ontology advise_ontology propose_ontology object_types explore_objects list_proposals get_proposal query_inferred explain_fact verify_claims check_slice certify_reasoning validate_shapes induce_shape check_shapes lint_schema list_packs show_pack plan_pack install_pack align_schema induce_sheet` |
| `reasoning` (16) | `ask search get_entity neighbors find_entities explain_blockers impact dependencies critical_path what_if get_schema query_inferred explain_fact verify_claims check_slice certify_reasoning` |
| `governance` (19) | `ask search get_entity history changes data_quality list_actions plan_action get_schema plan_schema_change propose_schema_change list_proposals get_proposal verify_claims certify_reasoning validate_shapes lint_schema plan_pack install_pack` |
| `authoring` (23) | `ask search get_entity get_schema plan_schema_change propose_schema_change draft_ontology refine_ontology advise_ontology propose_ontology object_types list_proposals get_proposal validate_shapes induce_shape check_shapes lint_schema list_packs show_pack plan_pack install_pack align_schema induce_sheet` |
| `catalog` (17) | `ask search get_entity neighbors find_data join_path trusted_metric authority learned_facts find_entities metric history changes experts get_schema object_types explore_objects` |
| `retrieval` (17) | `ask search get_entity neighbors find_data trusted_metric find_entities metric history changes experts get_schema object_types explore_objects query_inferred explain_fact check_slice` |
| `evaluation` (14) | `ask search get_entity metric data_quality what_if get_schema query_inferred explain_fact verify_claims check_slice certify_reasoning validate_shapes lint_schema` |

An unknown profile refuses to start:

```text
$ echo '{}' | onto mcp --profile auditing
error: invalid: unknown MCP profile 'auditing' (valid: full, catalog, authoring, reasoning, governance, retrieval, evaluation)
```

Under any profile but `full`, `what_if` runs a scenario only when every one of
its queries is the action of a tool the profile offers, so a scenario cannot
reach what the profile withholds; a scenario runs at most 20 queries.

Tool descriptions carry the warnings an agent needs: derived is not asserted,
a clean run is not a consistency result, a certificate binds bytes, an
installed pack is a proposal, and the limits (`query_inferred` takes at most
8 atoms and returns at most 1,000 bindings).

## What is proved, measured, or an opinion

| You ask | You get back | Kind of answer | Checked by |
|---|---|---|---|
| What follows from the graph under the schema's own semantics? | a certificate citing built-in rules only | proof under the built-in table: `entailed` | `onto-cert check` re-derives every line |
| What follows under our rules or a pack's rules? | a certificate citing supplied rules | proof relative to assumptions: `entailed_under_supplied_rules` | `onto-cert check` |
| Is this certificate about the store as it is now? | a binding report | comparison of bytes now: `same_graph` or `different_graph` | the engine, under the caller's scope |
| Is this fact in the closure? | `derived` with its derivation | engine opinion until certified | `reason.certify` |
| Why does it hold? | justifications | `sufficient` and `minimal` re-verified; `complete` only when enumerated | the engine, by re-running the closure |
| Does this answer's claim hold? | a verdict per claim | lookup in the closure; `not_entailed` is absence, not falsity | the engine |
| Does a slice keep what the whole entails? | preserved, lost with missing premises | comparison of two closures | the engine |
| What would this schema change do? | consequences, conservativity, risk | computation over the current graph; risk is the planner's opinion | the engine |
| Does the data fit the shapes? | `conforms`, `violates` or `undetermined` | measurement | the engine |
| Is the schema well formed? | lint findings | measurement by heuristics; no finding is no proof | the engine |
| Which types of two schemas correspond? | scores, SSSOM | opinion | certification can refute a claimed match, never strengthen it |
| What does this sheet look like as an ontology? | an object type, links, a shape | measurement of these rows | the plan and apply that follow |
| Did packing keep the premises of derived facts? | preservation counts | measurement of the packed text | the engine |
| How sure is a derived fact? | trust provenance | propagated opinion (entity `confidence`) | the engine |

## Limitations

- **onto-cert is tested, not proved.** It is a small Rust crate whose tests
  accept good certificates and reject forged ones, including a property test
  over random small graphs that changes one byte of a conclusion; unlike
  open-ontologies' checker, it has no machine-checked proof of soundness.
- **A certificate proves derivations, not the input.** It shows that every
  derived fact follows from `asserted.tsv` under the cited rules. Whether
  `asserted.tsv` is the graph the engine read is an attested claim; binding
  compares it with the store now, not then.
- **The rules are Horn rules without negation.** There is no negation, no
  disjunction, no existential head, no arithmetic and no aggregate. A rule
  cannot say "unless" (the SaaS pack's `candidate_plan` says so in its
  description).
- **Not a consistency checker.** The reasoner never reports a contradiction:
  it derives facts and lists what it did not evaluate. Cardinality, endpoints,
  property types and shapes are measured elsewhere or not at all.
- **Derived facts are not stored.** They cannot be queried with the stored
  graph APIs (`entity.get`, search); use `reason.query`, or the per-entity
  rule walk that `ask` and `context` already use for chain rules (it does not
  evaluate `when`/`then` rules).
- **The closure is computed in memory, per caller.** It is cached (four
  closures), bounded by `max_derived`, `max_rounds` and `max_work`, queries
  by their atom count, limit and join work, and plans skip consequences above
  500,000 asserted facts. A run cut by a bound is a lower bound and says so.
- **States and properties enter reasoning only when a rule reads them.** A
  rule can classify by a state or a property value; it cannot derive one.
- **A reserved word cannot name a relation.** The enterprise schema's
  `implemented_by` has the inverse name `implements`; in rule and query atoms
  that name means the inverse link, so the reasoner's own `implements` facts
  cannot be asked for by pattern (lint warns about it).
- **Certificates cover what the caller may read.** A certificate written by a
  member says nothing about entities the member cannot see, and a binding
  check run by someone else compares their scope, not the writer's.
- **Shapes are a subset of SHACL.** No SPARQL constraints, no qualified value
  shapes, no logical combinators (`and`, `or`, `not`, `xone`); unknown keys
  make a report `undetermined`.
- **Alignment compares names and structure, not meaning.** Link-type matches
  lean on endpoint agreement, and two schemas that share the enterprise
  vocabulary (person, team) agree on endpoints easily: read borderline and
  even accepted link matches as candidates. Certification can only refute.
- **Induction describes the sheet in front of it.** It does not know which
  columns should be required; a new link column is recognized only when at
  least 90% of its values already exist as objects of one kind (ingesting
  into an existing type also takes a column named like one of its link
  types, or a `--mapping`).
- **Packs borrow vocabulary.** A pack never claims compliance with the
  standards it names, and its rules flag, they do not decide (coverage,
  clinical or legal questions stay with people).
