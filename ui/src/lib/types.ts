// Shapes returned by the onto-intelli HTTP API (see crates/onto-core).

export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

export interface Entity {
  id: string;
  kind: string;
  workspace: string;
  title: string;
  state?: string;
  props?: Record<string, Json>;
  aliases?: string[];
  created_at: number;
  updated_at: number;
  revision: number;
  derived?: Record<string, Json>;
}

export interface Edge {
  from: string;
  rel: string;
  to: string;
}

export interface Subgraph {
  nodes: Entity[];
  edges: Edge[];
  truncated: boolean;
}

export interface KindCount {
  kind: string;
  count: number;
  label: string;
  color?: string;
  icon?: string;
}

export interface Overview extends Subgraph {
  kinds: KindCount[];
  total: number;
}

export interface Hop {
  from: string;
  rel: string;
  to: string;
}

export interface DepNode {
  id: string;
  kind: string;
  title: string;
  state?: string;
  open: boolean;
  alert: boolean;
  depth: number;
}

export interface DepReport {
  root: string;
  direction: string;
  nodes: DepNode[];
  edges: Hop[];
  open: string[];
  cycles: string[][];
}

export interface RootCause {
  root: string;
  chains: { culprit: DepNode; hops: Hop[]; text: string }[];
  visited: number;
}

export interface PropertyDef {
  type: string;
  required?: boolean;
  values?: string[];
  searchable?: boolean;
  description?: string;
  pattern?: string;
  max_length?: number;
}

export interface ObjectType {
  name: string;
  label?: string;
  description?: string;
  color?: string;
  icon?: string;
  category: string;
  extends?: string;
  properties?: Record<string, PropertyDef>;
  states?: string[];
  terminal_states?: string[];
  alert_states?: string[];
  implements?: string[];
  derived?: Record<string, Json>;
  constraints?: Record<string, Json>;
  backing?: Backing;
}

export interface LinkType {
  name: string;
  label?: string;
  description?: string;
  from?: string[];
  to?: string[];
  inverse?: string;
  cardinality: "many" | "one";
  dependency?: boolean;
  symmetric?: boolean;
  provenance?: boolean;
  weight: number;
}

export interface ParamDef {
  type: string;
  kinds?: string[];
  values?: string[];
  required?: boolean;
  default?: Json;
  description?: string;
  cue?: string[];
}

export interface ActionType {
  name: string;
  description?: string;
  phrases?: string[];
  roles?: string[];
  risk: "low" | "medium" | "high";
  parameters: Record<string, ParamDef>;
  preconditions?: Json[];
  effects: Json[];
}

export interface Rule {
  name: string;
  description?: string;
  chain: string[];
  infer: string;
}

export interface InterfaceType {
  name: string;
  description?: string;
  properties?: Record<string, PropertyDef>;
  links?: string[];
}

export interface SchemaDoc {
  name: string;
  version: number;
  schema_version?: number;
  object_types: Record<string, ObjectType>;
  link_types: Record<string, LinkType>;
  interfaces: Record<string, InterfaceType>;
  action_types: Record<string, ActionType>;
  rules: Rule[];
  metrics: Record<string, Json>;
  glossary: { kinds: Record<string, string>; relations: Record<string, string>; dimensions: Record<string, string> };
  analysis: Json;
}

export type SchemaChange = { op: string; [k: string]: Json | undefined };

export interface DiffEntry {
  section: string;
  name: string;
  change: "added" | "removed" | "modified";
  fields?: string[];
  before?: Json;
  after?: Json;
}

export interface ChangeImpact {
  index: number;
  op: string;
  target: string;
  summary: string;
  breaking: boolean;
  reasons?: string[];
  entities: number;
  links: number;
  sample?: string[];
}

export interface SchemaPlan {
  base_version: number;
  valid: boolean;
  errors?: string[];
  breaking: boolean;
  needs_migration?: string[];
  warnings?: string[];
  changes: ChangeImpact[];
  diff: { entries: DiffEntry[] };
  entities_affected: number;
  links_affected: number;
  migration: {
    entities_updated: number;
    entities_deleted: number;
    entities_retyped: number;
    links_deleted: number;
    links_moved: number;
    mutations: number;
  };
  /** Facts that appear and disappear from the closure (absent on plans stored before the field). */
  consequences?: Consequences;
  risk?: Risk;
  risk_reasons?: string[];
  locked_violations?: LockViolation[];
  likely_renames?: LikelyRename[];
  /** Shape results the change creates and resolves on today's graph (`schema.plan`). */
  shapes?: PlanShapeDelta;
}

export interface Review {
  by: string;
  decision: "approve" | "reject" | "comment";
  comment?: string;
  ts: number;
  at: string;
}

export interface Proposal {
  id: string;
  title: string;
  rationale: string;
  change_set: { changes: SchemaChange[]; message?: string; migrations?: Json[] };
  proposer: { id: string; roles: string[]; agent: boolean; entity?: string };
  status: "open" | "applied" | "rejected" | "failed" | "withdrawn";
  base_version: number;
  plan: SchemaPlan;
  reviews: Review[];
  applied_version?: number;
  error?: string;
  source?: string;
  created_at: number;
  updated_at: number;
  revision: number;
  approvals: number;
  required_approvals: number;
  stale: boolean;
  preview?: SchemaPlan;
  active_version: number;
}

export interface HistoryEntry {
  revision: number;
  ts: number;
  at: string;
  actor: string;
  deleted: boolean;
  state?: string;
  title?: string;
  changed: string[];
}

export interface SchemaVersionInfo {
  version: number;
  parent?: number;
  ts: number;
  at: string;
  actor: string;
  source: string;
  message?: string;
  proposal?: string;
  breaking: boolean;
  changes: number;
  diff: string[];
}

export interface Whoami {
  principal: { id: string; roles: string[]; workspaces: string[]; entity?: string; via?: string };
  tenant: string;
  tenants: string[];
  multi_tenant: boolean;
  default_workspace: string;
  dev_mode: boolean;
  version: string;
  schema_version: number;
  capabilities: { read: boolean; write: boolean; apply_schema: boolean; approve: boolean; propose: boolean; observe?: boolean };
  console?: { user: string; role: string; org: string } | null;
  credential?: string;
  ui: { title: string };
}

// ── ontology builder (crates/onto-core/src/modeling.rs) ──────────────────

export type PropType = "string" | "text" | "int" | "float" | "bool" | "timestamp" | "enum" | "list" | "json";

export interface DraftProperty {
  name: string;
  column: string;
  data_type: string;
  ty: PropType;
  required: boolean;
  values: string[];
  searchable: boolean;
  unique: boolean;
  pattern?: string;
  description: string;
  reason: string;
}

export interface DraftObject {
  name: string;
  label: string;
  description: string;
  table: string;
  table_id: string;
  category: string;
  key: string[];
  natural_key: boolean;
  title: string | null;
  state: string | null;
  states: string[];
  terminal_states: string[];
  alert_states: string[];
  properties: DraftProperty[];
  sync: boolean;
  confidence: number;
  reasons: string[];
}

export interface DraftLink {
  name: string;
  inverse: string;
  label: string;
  description: string;
  from: string;
  to: string;
  cardinality: "one" | "many";
  via: "foreign_key" | "join_table" | "learned";
  column: string | null;
  target_column?: string;
  table?: string;
  confidence: number;
  reason: string;
}

export interface DraftAction {
  name: string;
  object: string;
  kind: "create" | "edit" | "transition" | "relink";
  risk: "low" | "medium" | "high";
  description: string;
  phrases: string[];
  state?: string;
  link?: string;
  reason: string;
}

export interface OntologyDraft {
  source: string;
  changes: { changes: SchemaChange[]; message?: string; migrations?: Json[] };
  objects: DraftObject[];
  links: DraftLink[];
  actions: DraftAction[];
  skipped: { table: string; reason: string }[];
  notes: string[];
}

export interface Suggestion {
  target: string;
  field: string;
  from: Json;
  to: Json;
  why: string;
}

export interface Refined {
  draft: OntologyDraft;
  suggestions: Suggestion[];
  applied: number;
  rejected: { suggestion: Suggestion; error: string }[];
  llm: string;
  tokens: number;
}

export interface Advice {
  answer: string;
  mode: "deterministic" | "llm";
  facts: string[];
  source: string;
  tokens: number;
  note?: string;
}

// ── connectors (crates/onto-intelli/src/connectors.rs) ───────────────────

export interface ConnectorSpec {
  name: string;
  kind: string;
  url: string;
  workspace?: string;
  title: string;
  schemas: string[];
  exclude_schemas: string[];
  every_secs: number;
  map_grants: boolean;
  created_by: string;
  created_at: number;
}

export interface ObjectSync {
  rows: number;
  upserted: number;
  unchanged: number;
  deleted: number;
  invalid?: number;
  truncated?: boolean;
}

export interface CrawlReport {
  connector: string;
  source: string;
  started_at: number;
  ms: number;
  schemas: number;
  tables: number;
  views: number;
  columns: number;
  foreign_keys: number;
  queries: number;
  upserted: number;
  unchanged: number;
  deleted: number;
  links_removed: number;
  ranked: number;
  objects?: Record<string, ObjectSync>;
  links?: Record<string, { rows: number; linked: number; unlinked: number; skipped?: number }>;
  notes?: string[];
}

export interface ConnectorInfo {
  spec: ConnectorSpec;
  last_crawl_at: number;
  last_success_at: number;
  last_error?: string | null;
  crawls: number;
  assets: number;
  last?: CrawlReport | null;
}

// ── Object Explorer (crates/onto-core/src/explorer.rs) ───────────────────

export interface Backing {
  source: string;
  table: string;
  key: string[];
  sync?: boolean;
  title?: string;
  state?: string;
  properties?: Record<string, string>;
  links?: Record<string, string>;
  max_rows?: number;
}

export interface ObjectTypeSummary {
  name: string;
  label: string;
  category: string;
  count: number;
  backing?: Backing;
  properties: number;
  links: string[];
  actions: string[];
  description: string;
}

export type FilterOp = "eq" | "ne" | "in" | "gt" | "gte" | "lt" | "lte" | "contains" | "exists" | "missing";

export interface Filter {
  prop: string;
  op: FilterOp;
  value?: Json;
}

export interface Stats {
  count: number;
  min: number;
  max: number;
  mean?: number;
  sum?: number;
  histogram?: { lo: number; hi: number; count: number }[];
}

export interface ExploreResult {
  kind: string;
  label: string;
  total: number;
  objects: Entity[];
  columns: { name: string; label: string; ty: PropType }[];
  facets: Record<string, { value: Json; count: number }[]>;
  stats: Record<string, Stats>;
  links: { rel: string; label: string; to: string[] }[];
  actions: string[];
}

// ── action types at run time (crates/onto-core/src/action_types.rs) ──────

export interface ActionPlan {
  action: string;
  params: Record<string, Json>;
  valid: boolean;
  checks: { ok: boolean; check: string; message: string }[];
  preview: string[];
  risk: "low" | "medium" | "high";
  reversible: boolean;
  needs_confirmation: boolean;
  warnings: string[];
}

export interface ActionReceipt {
  id: string;
  action: string;
  status: "applied" | "proposed" | "rejected" | "undone";
  actor: string;
  ts: number;
  plan: ActionPlan;
  reason?: string;
}

// ── assurance (crates/onto-core/src: reason.rs, explain.rs, shapes.rs, lint.rs, locks.rs, packs.rs, live_schema.rs; crates/onto-cert) ──

export type Risk = "low" | "medium" | "high";

/** A triple of spelled terms: `<issue:421>`, `#team`, `depends_on`, `prop.priority`, `"high"`. */
export interface Fact {
  s: string;
  p: string;
  o: string;
}

/** One recorded derivation: the rule and its premises in rule-body order. */
export interface Derivation {
  rule: string;
  conclusion: Fact;
  premises: Fact[];
}

/** A fact one closure has and another has not, with how it was derived (none when asserted). */
export interface AddedFact {
  fact: Fact;
  rule?: string;
  premises?: Fact[];
}

export interface Refusal {
  term: string;
  reason: string;
}

export interface ReasonStats {
  workspace?: string;
  asserted: number;
  derived: number;
  rules: number;
  by_rule: Record<string, number>;
  rounds: number;
  fixpoint_reached: boolean;
  truncated?: string;
  refused: number;
  refusals?: Refusal[];
  /** Schema constructs the run did not evaluate: silence about them is not "valid". */
  not_evaluated: string[];
  instances?: number;
  instances_truncated?: boolean;
}

/** `reason.run`: the stats plus sample derivations. */
export interface ReasonRun extends ReasonStats {
  samples?: AddedFact[];
  means?: string;
}

/** `reason.query`: variable name (without `?`) → spelled term. */
export interface QueryAnswer {
  bindings: Record<string, string>[];
  fixpoint_reached: boolean;
  truncated?: string;
}

export type Entailment = "asserted" | "derived" | "not_entailed";

export interface Justification {
  facts: Fact[];
  rules: string[];
  builtin_rules?: string[];
  proof?: Derivation[];
  sufficient: boolean;
  minimal: boolean;
}

/** `reason.explain`. */
export interface Justifications {
  fact: Fact;
  status: Entailment;
  justifications: Justification[];
  complete: boolean;
  fixpoint_reached: boolean;
  stopped?: string;
  steps: number;
  verification?: string;
  means: string;
}

/** The four files of an `oi-cert/1` certificate. */
export interface Certificate {
  rules_tsv: string;
  asserted_tsv: string;
  derivations_tsv: string;
  manifest_json: string;
}

export type CertVerdict = "entailed" | "entailed_under_supplied_rules";

/** The `onto-cert` checker's report: the only place checker words appear. */
export type CertCheck =
  | {
      ok: true;
      verdict: CertVerdict;
      asserted: number;
      derivations: number;
      rules_cited: { builtin: number; supplied: number };
      rules_sha256: string;
      checker: string;
      means: string;
    }
  | {
      ok: false;
      error?: "malformed";
      reason: string;
      asserted?: number;
      derivations?: number;
      first_rejected?: number;
      rule?: string;
      premise?: number;
      conclusion?: string;
      premises?: string[];
      file?: string;
      line?: number;
      field?: string;
      expected?: Json;
      actual?: Json;
    };

/** `reason.certify`. */
export interface Certified {
  certificate: Certificate;
  check: CertCheck;
}

/** `manifest.json` of a certificate. */
export interface CertManifest {
  format: string;
  derived: { asserted_sha256: string; rules_sha256: string; derivations_sha256: string; asserted: number; rules: number; derivations: number };
  attested: {
    engine: string;
    created_at: string;
    workspace?: string | null;
    schema_version: number;
    graph_revision: number;
    principal: string;
    fixpoint_reached: boolean;
    rounds: number;
  };
  means: string;
}

// shapes

export type ShapeSeverity = "violation" | "warning" | "info";
export type ShapeVerdict = "conforms" | "violates" | "undetermined";

export interface ShapeResult {
  shape: string;
  constraint_id: string;
  component: string;
  focus: string;
  path?: string;
  value?: Json;
  severity: ShapeSeverity;
  message: string;
}

export interface SkippedConstraint {
  shape: string;
  constraint_id?: string;
  key: string;
  reason: string;
}

/** `shapes.validate`: a measurement. */
export interface ShapeReport {
  verdict: ShapeVerdict;
  conforms: boolean | null;
  results: ShapeResult[];
  total_results: number;
  truncated?: boolean;
  focus_by_shape: Record<string, number>;
  focus_nodes: number;
  unmatched_targets?: string[];
  skipped?: SkippedConstraint[];
  counts: { violation: number; warning: number; info: number };
  workspace?: string;
  inferred: boolean;
  fixpoint_reached?: boolean;
  means: string;
}

/** A plan's shape delta. `new_results`/`resolved` are lists (shapes::delta) or counts with `samples`. */
export interface PlanShapeDelta {
  before?: ShapeVerdict;
  after?: ShapeVerdict;
  new_results?: ShapeResult[] | number;
  resolved?: ShapeResult[] | number;
  samples?: ShapeResult[];
  complete?: boolean;
  skipped?: string;
  means?: string;
}

// semantic plans (live_schema.rs, locks.rs)

export type Conservativity = "conservative" | "not_conservative" | "undetermined";

export interface Consequences {
  added: number;
  removed: number;
  conservative: boolean;
  verdict?: Conservativity;
  new_over_existing_vocabulary: number;
  by_rule?: Record<string, number>;
  samples_added?: AddedFact[];
  witnesses?: AddedFact[];
  samples_removed?: Fact[];
  fixpoint_reached: boolean;
  skipped?: string;
  means: string;
}

export interface LockViolation {
  section: string;
  name: string;
  change: string;
  touch: "delete" | "rename" | "breaking_change";
  reason: string;
  renamed_to?: string;
  lock_reason?: string;
  locked_by: string;
  overridden: boolean;
}

export interface LikelyRename {
  section: string;
  from: string;
  to: string;
  score: number;
  evidence?: string[];
}

export interface SchemaLock {
  section: string;
  name: string;
  reason: string;
  locked_by: string;
  at: string;
}

// lint

export type LintSeverity = "error" | "warning" | "info";

export interface LintFinding {
  id: string;
  check: string;
  severity: LintSeverity;
  target: string;
  message: string;
  fix: string;
  accepted?: boolean;
  dismissed?: boolean;
  note?: string;
}

/** `schema.lint`: a measurement. */
export interface LintReport {
  findings: LintFinding[];
  counts: { error: number; warning: number; info: number };
  suppressed: number;
  checks_run: string[];
  not_run?: { check: string; reason: string }[];
  partial?: { check: string; reason: string }[];
  schema_version?: number;
  means: string;
}

export interface LintFeedback {
  id: string;
  check: string;
  target: string;
  decision: "accept" | "dismiss";
  note?: string;
  by: string;
  at: number;
}

// packs

export interface PackMeta {
  id: string;
  title: string;
  industry?: string;
  version: string;
  description?: string;
  requires?: string[];
  standards?: string[];
  sample_note?: string;
}

export interface PackCounts {
  object_types: number;
  link_types: number;
  interfaces: number;
  rules: number;
  shapes: number;
  metrics: number;
  action_types: number;
  glossary_terms: number;
  sample_entities: number;
}

/** `pack.list` item. */
export interface PackInfo extends PackMeta {
  counts: PackCounts;
  installed: boolean;
  pending_changes: number;
  missing_requires?: string[];
  blocked?: string;
}

export interface PackItem {
  name: string;
  label?: string;
  description?: string;
}

export interface PackLink {
  name: string;
  description?: string;
  from: string[];
  to: string[];
  inverse?: string;
  transitive?: boolean;
  dependency?: boolean;
}

export interface PackRule {
  name: string;
  description?: string;
  form: "chain" | "horn";
  text: string;
}

/** `pack.show`: everything a pack defines, plus a lint summary of it layered on the current schema. */
export interface PackDetail extends PackMeta {
  counts: PackCounts;
  object_types: PackItem[];
  link_types: PackLink[];
  interfaces: PackItem[];
  rules: PackRule[];
  derived_classes: string[];
  derived_relations: string[];
  shapes: PackItem[];
  metrics: PackItem[];
  action_types: PackItem[];
  glossary: Record<string, string>;
  toml: string;
  means: string;
  lint?: Partial<LintReport> & { counts?: { error: number; warning: number; info: number } };
}

export type InstallMode = "apply" | "propose";

/** `pack.install`. */
export interface PackInstalled {
  pack: string;
  pack_version: string;
  mode: InstallMode;
  already_installed: boolean;
  changes: number;
  version?: SchemaVersionInfo;
  proposal?: Proposal;
  sample_entities: number;
  sample_links: number;
  sample_links_skipped: number;
  revision?: number;
}
