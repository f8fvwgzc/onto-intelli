// The console API client (`/v1/console/…`, docs/CONTROL_PLANE.md section 10):
// sign-in, organizations, members, projects, API keys, LLM settings, usage.
// Same `parse` and error handling as api.ts; console routes need only the
// bearer token (they name their organization or project in the path).

import { useQuery } from "@tanstack/react-query";

import { ApiError, parse } from "./api";
import { getSession, useSession } from "./session";

export type OrgRole = "owner" | "admin" | "developer" | "viewer";
export type KeyScope = "read" | "write" | "agent" | "admin";

export const ROLES: OrgRole[] = ["owner", "admin", "developer", "viewer"];
const RANK: Record<OrgRole, number> = { viewer: 0, developer: 1, admin: 2, owner: 3 };

/** `role` has at least the rights of `min` (section 5). */
export function atLeast(role: OrgRole | undefined, min: OrgRole): boolean {
  return role !== undefined && RANK[role] >= RANK[min];
}

/** The tooltip on a control a role cannot use. */
export function needs(min: OrgRole): string {
  return `Needs the ${min} role${min === "owner" ? "" : " or higher"}`;
}

export interface ConsoleUser {
  id: string;
  email?: string | null;
  email_verified: boolean;
  name: string;
  avatar_url?: string | null;
  created_at: number;
  last_login_at?: number | null;
}

export interface Org {
  id: string;
  slug: string;
  name: string;
  created_at: number;
  created_by: string;
  role: OrgRole;
}

export interface ProjectSettings {
  default_workspace?: string;
  trace_retention_days?: number;
  capture_content?: boolean;
  asks_per_minute?: number;
  [k: string]: unknown;
}

export interface Project {
  id: string;
  org_id: string;
  ref: string;
  name: string;
  status: string;
  settings: ProjectSettings;
  settings_version: number;
  created_at: number;
  created_by: string;
  deleted_at?: number | null;
  role: OrgRole;
  seeded?: { dataset: string; workspace: string; entities: number; links: number; events: number } | null;
}

export interface Me {
  user: ConsoleUser;
  orgs: Org[];
  projects: Project[];
}

export interface Providers {
  providers: { id: string; name: string }[];
  bootstrap: boolean;
  dev_mode: boolean;
  signup: boolean;
}

export interface SignedIn {
  token: string;
  expires_at: number;
  user: ConsoleUser;
  org?: Org;
}

export interface ApiKey {
  id: string;
  project_id: string;
  name: string;
  prefix: string;
  scopes: KeyScope[];
  workspaces: string[];
  impersonate: boolean;
  impersonate_roles: string[];
  created_at: number;
  created_by: string;
  expires_at?: number | null;
  revoked_at?: number | null;
  revoked_by?: string | null;
  last_used_at?: number | null;
  rotated_from?: string | null;
}

export interface NewKey {
  name: string;
  scopes: KeyScope[];
  workspaces?: string[];
  expires_at?: number;
  impersonate?: boolean;
  impersonate_roles?: string[];
}

export interface Member {
  user_id: string;
  email?: string | null;
  name: string;
  avatar_url?: string | null;
  role: OrgRole;
  joined_at: number;
}

export interface Invitation {
  id: string;
  org_id: string;
  email: string;
  role: OrgRole;
  invited_by: string;
  created_at: number;
  expires_at: number;
  accepted_at?: number | null;
  accepted_by?: string | null;
  revoked_at?: number | null;
}

export interface AuditEntry {
  seq: number;
  ts: number;
  org_id?: string | null;
  project_id?: string | null;
  actor: string;
  action: string;
  target?: string | null;
  detail: Record<string, unknown>;
}

export interface Pricing {
  input_per_mtok: number;
  output_per_mtok: number;
  cached_input_per_mtok: number;
}

export interface LlmSettings {
  provider: string;
  model: string;
  base_url?: string | null;
  api_key_last4?: string | null;
  has_api_key: boolean;
  max_output_tokens: number;
  temperature?: number | null;
  effort?: string | null;
  timeout_secs: number;
  pricing: Pricing;
  headers: Record<string, string>;
  monthly_budget_usd?: number | null;
  monthly_token_budget?: number | null;
  asks_per_minute?: number | null;
  updated_at: number;
  updated_by: string;
}

export interface LlmView {
  llm: LlmSettings | null;
  error: string | null;
  master_key: boolean;
  server_default: { provider: string; model: string };
}

/** The body of `PUT …/llm`: `api_key` absent keeps the stored key, `""` removes it. */
export interface LlmUpdate {
  provider: string;
  model?: string;
  base_url?: string | null;
  api_key?: string;
  max_output_tokens?: number;
  temperature?: number;
  effort?: string;
  timeout_secs?: number;
  pricing?: Pricing;
  headers?: Record<string, string>;
  monthly_budget_usd?: number;
  monthly_token_budget?: number;
  asks_per_minute?: number;
}

export interface LlmTest {
  ok: boolean;
  latency_ms: number;
  provider: string;
  model: string;
  response_model?: string | null;
  usage?: { input_tokens?: number; output_tokens?: number } | null;
  error?: string | null;
}

export interface UsageRow {
  project_id: string;
  day: string;
  provider: string;
  model: string;
  asks: number;
  llm_calls: number;
  deterministic: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  cost_usd: number;
  errors: number;
}

export interface Usage {
  from: string;
  to: string;
  rows: UsageRow[];
  month_to_date: { month: string; cost_usd: number; tokens: number };
  budget: {
    monthly_budget_usd?: number | null;
    monthly_token_budget?: number | null;
    asks_per_minute?: number | null;
    reached?: string | null;
  };
}

export interface Connect {
  api_url: string;
  grpc_url: string;
  mcp_url: string;
  ref: string;
  keys: { prefix: string; name: string; scopes: KeyScope[] }[];
}

function headers(extra: Record<string, string> = {}): HeadersInit {
  const h: Record<string, string> = { "content-type": "application/json", ...extra };
  // Public routes pass `authorization: ""`: no token is sent.
  const t = getSession().token;
  if (!("authorization" in extra) && t) h.authorization = `Bearer ${t}`;
  if (!h.authorization) delete h.authorization;
  return h;
}

async function req<T>(method: string, path: string, body?: unknown, extra?: Record<string, string>): Promise<T> {
  const res = await fetch(`/v1/console${path}`, {
    method,
    headers: headers(extra),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return parse<T>(res);
}

const enc = encodeURIComponent;

export const consoleApi = {
  /** Public: the sign-in paths this server offers. `null` when the console is off. */
  async providers(): Promise<Providers | null> {
    try {
      const res = await fetch("/v1/console/auth/providers");
      if (!res.ok || !(res.headers.get("content-type") ?? "").includes("json")) return null;
      return (await res.json()) as Providers;
    } catch {
      return null;
    }
  },
  /** Where the sign-in button goes: a 302 to the IdP. */
  startUrl(provider: string, binding: string, returnTo: string, invite?: string): string {
    const q = new URLSearchParams({ binding, return_to: returnTo });
    if (invite) q.set("invite", invite);
    return `/v1/console/auth/oidc/${enc(provider)}/start?${q}`;
  },
  exchange: (ticket: string, binding: string) => req<SignedIn>("POST", "/auth/exchange", { ticket, binding }, { authorization: "" }),
  bootstrap: (token: string) => req<SignedIn>("POST", "/auth/token", undefined, { authorization: `Bearer ${token}` }),
  dev: () => req<SignedIn>("POST", "/auth/dev", undefined, { authorization: "" }),
  logout: (everywhere: boolean) => req<{ ok: boolean }>("POST", "/auth/logout", { everywhere }),
  me: () => req<Me>("GET", "/me"),
  invitation: (code: string) =>
    req<{ org: { id: string; name: string; slug: string }; role: OrgRole; email: string; expires_at: number }>("GET", `/invitations/${enc(code)}`, undefined, {
      authorization: "",
    }),

  createOrg: (name: string) => req<Org>("POST", "/orgs", { name }),
  renameOrg: (org: string, name: string) => req<Org>("PATCH", `/orgs/${enc(org)}`, { name }),
  members: (org: string) => req<{ members: Member[] }>("GET", `/orgs/${enc(org)}/members`),
  setRole: (org: string, user: string, role: OrgRole) => req<unknown>("PATCH", `/orgs/${enc(org)}/members/${enc(user)}`, { role }),
  removeMember: (org: string, user: string) => req<unknown>("DELETE", `/orgs/${enc(org)}/members/${enc(user)}`),
  invitations: (org: string) => req<{ invitations: Invitation[] }>("GET", `/orgs/${enc(org)}/invitations`),
  invite: (org: string, email: string, role: OrgRole) => req<{ invitation: Invitation; link: string }>("POST", `/orgs/${enc(org)}/invitations`, { email, role }),
  revokeInvitation: (org: string, id: string) => req<unknown>("DELETE", `/orgs/${enc(org)}/invitations/${enc(id)}`),
  audit: (org: string, before?: number) => req<{ entries: AuditEntry[]; next: number | null }>("GET", `/orgs/${enc(org)}/audit?limit=50${before ? `&before=${before}` : ""}`),

  projects: (org: string) => req<{ projects: Project[] }>("GET", `/orgs/${enc(org)}/projects`),
  createProject: (org: string, name: string, seed?: "demo") => req<Project>("POST", `/orgs/${enc(org)}/projects`, { name, seed }),
  updateProject: (ref: string, patch: { name?: string; settings?: Record<string, unknown> }) => req<Project>("PATCH", `/projects/${enc(ref)}`, patch),
  deleteProject: (ref: string, confirm: string) => req<unknown>("DELETE", `/projects/${enc(ref)}`, { confirm }),

  keys: (ref: string) => req<{ keys: ApiKey[] }>("GET", `/projects/${enc(ref)}/keys`),
  createKey: (ref: string, k: NewKey) => req<{ key: ApiKey; secret: string }>("POST", `/projects/${enc(ref)}/keys`, k),
  rotateKey: (ref: string, id: string, grace_secs: number) => req<{ key: ApiKey; secret: string }>("POST", `/projects/${enc(ref)}/keys/${enc(id)}/rotate`, { grace_secs }),
  revokeKey: (ref: string, id: string) => req<{ key: ApiKey }>("DELETE", `/projects/${enc(ref)}/keys/${enc(id)}`),

  llm: (ref: string) => req<LlmView>("GET", `/projects/${enc(ref)}/llm`),
  putLlm: (ref: string, u: LlmUpdate) => req<LlmView>("PUT", `/projects/${enc(ref)}/llm`, u),
  deleteLlm: (ref: string) => req<{ deleted: boolean }>("DELETE", `/projects/${enc(ref)}/llm`),
  testLlm: (ref: string, draft?: LlmUpdate) => req<LlmTest>("POST", `/projects/${enc(ref)}/llm/test`, draft ?? {}),
  usage: (ref: string, from?: string, to?: string) => req<Usage>("GET", `/projects/${enc(ref)}/usage?${new URLSearchParams({ ...(from ? { from } : {}), ...(to ? { to } : {}) })}`),
  connect: (ref: string) => req<Connect>("GET", `/projects/${enc(ref)}/connect`),
};

// ── queries ──────────────────────────────────────────────────────────────────

export function useProviders(enabled: boolean) {
  return useQuery({ queryKey: ["console", "providers"], queryFn: consoleApi.providers, enabled, staleTime: 60_000, retry: false });
}

export function useMe() {
  const s = useSession();
  return useQuery<Me>({
    queryKey: ["console", "me", s.token ?? ""],
    queryFn: consoleApi.me,
    enabled: Boolean(s.token && s.console),
    retry: (n, e) => !(e instanceof ApiError && e.status < 500) && n < 2,
    staleTime: 30_000,
  });
}

/** The selected project and organization, with the caller's roles there. */
export function useConsoleScope() {
  const s = useSession();
  const me = useMe().data;
  const project = me?.projects.find((p) => p.ref === s.tenant);
  const org = me?.orgs.find((o) => o.id === project?.org_id) ?? me?.orgs.find((o) => o.id === s.org) ?? me?.orgs[0];
  return { me, project, org, role: project?.role, orgRole: org?.role };
}

export const qk = {
  keys: (ref: string) => ["console", "keys", ref] as const,
  llm: (ref: string) => ["console", "llm", ref] as const,
  usage: (ref: string, from: string, to: string) => ["console", "usage", ref, from, to] as const,
  connect: (ref: string) => ["console", "connect", ref] as const,
  projects: (org: string) => ["console", "projects", org] as const,
  members: (org: string) => ["console", "members", org] as const,
  invitations: (org: string) => ["console", "invitations", org] as const,
  audit: (org: string) => ["console", "audit", org] as const,
};
