# External signals and pattern recommendations

Status: first slice built (`crates/onto-intelli/src/signals.rs`). This note
covers what it does, what it deliberately does not do, and what comes next.

## The problem

What happens outside an organization (a supplier's outage, a regulation, a
merger, a security advisory) often explains or predicts what happens in its
graph. An ontology that only sees its own records misses those patterns. The
records it would need (who depends on whom, which incidents follow which
announcements) are not modelled.

Two things are wanted:

1. **Signals in, safely.** News and feeds come from the open internet. They
   must never become graph nodes on their own: an article is not an entity,
   and a feed that writes entities would let anyone who controls a website
   write into the graph. Fetching them must not open an SSRF hole.
2. **Suggestions out, governed.** What the signals suggest about the model
   (a missing link type, an inference rule) goes through the same proposal
   flow as any other schema change: people review and apply it. The
   recommender never changes the schema or the graph itself.

## Decisions

| Decision | Why |
|---|---|
| A feed item becomes a `signal.item` **event**, and by default a **knowledge document** (collection `signals:<feed>`). It never becomes an entity. | Events are the audit trail of what came in, filtered by workspace like every event. Knowledge makes items searchable with citations (`knowledge.search`). Neither touches the graph, entity search, reasoning or the change feed. |
| Feeds are registered and fetched by **admins only**, never agents. | Fetching is outbound traffic chosen by a caller. |
| Fetching follows `crate::net`'s rules for project-registered targets: `https` only, **public addresses only** (operator allow rules do not apply), the resolver checks every DNS answer right before connecting, no proxy, at most **three redirects, each checked again**, 4 MiB, 15 s. `[signals] allow_private_urls` (development) also reaches loopback and private ranges; link-local and metadata addresses are refused whatever the setting. | The same guard as webhooks and writeback (docs/design/EVENTS.md B.5), so there is one SSRF model to audit. Redirects are followed by hand, because the guarded client never follows them, and an open redirect on a public site must not lead inside. |
| Fetches run **on demand** (`signals.fetch`). | A scheduler already exists elsewhere: texc-symphony, or an automation timer. A poller inside onto-intelli would need leases across replicas, and the outbox already shows how much that costs. |
| Items are deduplicated per feed by a hash of the guid (else link, else title). The last 5,000 hashes are kept on the feed. | Feeds repeat their items on every fetch. |
| The recommender **reads**: `signals.recommend` returns suggestions. Filing them is a separate write, `signals.propose`, which goes through `schema.propose` with `source = "signals"`. | People decide. An open proposal with the same title is not filed again. The proposal's plan, risk and approvals work as for any proposal. |
| Only entities the caller may read are matched, and only `signal.item` events the caller may read are counted. | A suggestion must not reveal a restricted entity through its evidence. |

## What the recommender suggests (first slice)

**Mentions.** An item names an entity when the entity's whole title appears
in the item's title or summary, as whole words, ignoring case. Titles of one
short word (under 4 characters) and of more than 6 words are not matched:
they name too much, or almost nothing.

**Link types.** For each pair of kinds `(A, B)`, the recommender counts the
items that name an entity of `A` and one of `B` that are not already linked
in the graph. When at least `min_support` items (default 3) do, and the
schema has no link type naming both kinds, it suggests `put_link_type
<a>_named_with_<b>` (from `A` to `B`, symmetric when `A = B`). The evidence
is up to five items with their links. The name is deliberately plain; a
reviewer renames it (`schema.revise`) when a sharper relation fits ("supplies",
"competes_with").

**Chain rules.** For the same unlinked pairs, it looks for two-step paths
`a -r1-> m -r2-> b` in the graph. When the commonest `(r1, r2)` joins at
least `min_support` pairs, and no rule has that chain, it suggests `put_rule`
with `chain = [r1, r2]` and `infer = <a>_named_with_<b>`. `signals.propose`
files the rule in the same proposal as the link type it needs.

Each suggestion carries `support`, `evidence` and the exact `change_set` that
would be proposed, so a reviewer, or an agent calling `schema.propose`
itself, sees what would change.

## Actions

| Action | Who | Does |
|---|---|---|
| `signals.feed_add {name, url, workspace?, collection?, sensitivity?, events_only?}` | admins | registers a feed (URL checked now and on every fetch); audited as `signals.feed_add` |
| `signals.feeds` | readers | the feeds of the caller's workspaces, with `last_fetch`, `last_error` and `items` |
| `signals.feed_remove {name}` | admins | removes the feed; its events and documents stay |
| `signals.fetch {name}` | admins | fetches it now; returns `{items, stored, documents}` |
| `signals.recommend {workspace?, feeds?, since?, min_support?, limit?}` | readers (agents too) | suggestions, read-only |
| `signals.propose {…same…}` | who may propose schema changes, not agents | files the suggestions as proposals; returns `{proposed, skipped}` |

Item sensitivity defaults to `public`. A feed of internal material (a vendor
portal's RSS) can set `internal` or higher, and knowledge's rules then apply,
the agent ceiling included.

## Not done yet, in order

1. **Scheduled fetches** through an automation timer or texc-symphony, with
   `If-Modified-Since` and `ETag`. Until then, two fetches of one feed that
   overlap (two admins, two replicas) can both record an item: the seen-list
   is read and saved per fetch, not under a lease. A scheduler should not
   overlap them. Knowledge documents are not doubled, because an item's
   document id is derived from its guid.
2. **Better mentions:** aliases (`aka` properties), the entity resolver used by
   `intelli.ask`, and recognising organizations that are not entities yet,
   which would suggest a `put_object_type` or a new entity through an action
   type, never directly.
3. **More patterns:** properties (a recurring attribute in items about one
   kind, such as "recall" for products), shapes (items that contradict a
   constraint), and time patterns ("incidents on X follow advisories about Y
   within 7 days", which needs the change feed's history).
4. **Scoring:** support alone favours chatty feeds; lift over chance
   co-occurrence would rank better.
5. **Other sources:** JSON feeds, and webhooks from news APIs, delivered as
   `signal.item` events through `EventService/Ingest`.
