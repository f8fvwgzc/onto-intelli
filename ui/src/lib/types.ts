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
  capabilities: { read: boolean; write: boolean; apply_schema: boolean; approve: boolean; propose: boolean };
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
