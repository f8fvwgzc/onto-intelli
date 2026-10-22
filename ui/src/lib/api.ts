import { clearToken, getSession } from "./session";
import type { Json, Whoami } from "./types";

export class ApiError extends Error {
  status: number;
  code: string;
  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

/** The caller's credentials and scope: bearer token, tenant, workspace. */
export function headers(tenant?: string): HeadersInit {
  const s = { ...getSession(), ...(tenant ? { tenant, workspace: undefined } : {}) };
  const h: Record<string, string> = { "content-type": "application/json" };
  if (s.token) h.authorization = `Bearer ${s.token}`;
  if (s.tenant) h["x-onto-tenant"] = s.tenant;
  if (s.workspace) h["x-onto-workspace"] = s.workspace;
  return h;
}

/** The body of a response, or an `ApiError` from `{error: {code, message}}` (a 401 drops the token). */
export async function parse<T>(res: Response): Promise<T> {
  const text = await res.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    // Not JSON (a proxy error page): keep the status text.
  }
  if (!res.ok) {
    const err = body?.error ?? {};
    if (res.status === 401) clearToken();
    throw new ApiError(res.status, err.code ?? "error", err.message ?? res.statusText);
  }
  return body as T;
}

/** Run an engine action (`POST /v1/call/{action}`). */
export async function call<T = Json>(action: string, input: unknown = {}): Promise<T> {
  const res = await fetch(`/v1/call/${action}`, { method: "POST", headers: headers(), body: JSON.stringify(input) });
  return parse<T>(res);
}

/** `GET` a data-plane route (`/v1/traces`, `/v1/metrics`, …); empty query values are left out. */
export async function get<T = Json>(path: string, query: Record<string, string | number | undefined> = {}, tenant?: string): Promise<T> {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) if (v !== undefined && v !== "") q.set(k, String(v));
  const qs = q.toString();
  return parse<T>(await fetch(`${path}${qs ? `?${qs}` : ""}`, { headers: headers(tenant) }));
}

/** `POST` JSON to a data-plane route. */
export async function post<T = Json>(path: string, body: unknown = {}): Promise<T> {
  return parse<T>(await fetch(path, { method: "POST", headers: headers(), body: JSON.stringify(body) }));
}

export async function whoami(): Promise<Whoami> {
  return parse<Whoami>(await fetch("/v1/auth/whoami", { headers: headers() }));
}

export interface UiConfig {
  title: string;
  embed_origins: string[];
  version: string;
}

export async function uiConfig(): Promise<UiConfig> {
  try {
    const res = await fetch("/ui/config.json");
    if (res.ok) return (await res.json()) as UiConfig;
  } catch {
    // Dev server without a backend: defaults below.
  }
  return { title: "onto-intelli", embed_origins: [], version: "" };
}

export function errorText(e: unknown): string {
  if (e instanceof ApiError) return e.message.replace(/^(invalid|conflict|permission denied|not found): /, "");
  if (e instanceof Error) return e.message;
  return String(e);
}
