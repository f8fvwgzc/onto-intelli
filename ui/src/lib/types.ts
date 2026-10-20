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
