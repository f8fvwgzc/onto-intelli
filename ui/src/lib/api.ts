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

function headers(): HeadersInit {
  const s = getSession();
  const h: Record<string, string> = { "content-type": "application/json" };
  if (s.token) h.authorization = `Bearer ${s.token}`;
  if (s.tenant) h["x-onto-tenant"] = s.tenant;
  if (s.workspace) h["x-onto-workspace"] = s.workspace;
  return h;
}

async function parse<T>(res: Response): Promise<T> {
  const text = await res.text();
  const body = text ? JSON.parse(text) : null;
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
