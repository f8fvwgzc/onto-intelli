// Where the UI's credentials come from, in order:
// 1. a host page that embeds the UI posts {type: "onto:session", token, tenant, workspace}
//    (only from origins in [ui] embed_origins, or this origin);
// 2. a link with #…?token=…&tenant=…&workspace=… (the token is removed from the address at once);
// 3. the sign-in form (a token pasted by an operator);
// 4. with the console (docs/CONTROL_PLANE.md section 7), a console sign-in: an
//    IdP through OpenID Connect, the bootstrap admin token or dev mode. Its
//    `onto1.` session is marked `console`, and the selected project's ref is
//    the tenant, so the ontology pages work per project.
// Kept in sessionStorage: it ends with the tab.

import { useSyncExternalStore } from "react";

export interface Session {
  token?: string;
  tenant?: string;
  workspace?: string;
  /** The token is a console session (the shell shows organizations and projects). */
  console?: boolean;
  /** The selected organization (console). */
  org?: string;
  /** Unix seconds: when this tab received the console token, and when it expires. */
  issued_at?: number;
  expires_at?: number;
}

const KEY = "onto.session";
let current: Session = read();
const listeners = new Set<() => void>();

function read(): Session {
  try {
    const raw = sessionStorage.getItem(KEY);
    return raw ? (JSON.parse(raw) as Session) : {};
  } catch {
    return {};
  }
}

function write(s: Session) {
  current = s;
  try {
    sessionStorage.setItem(KEY, JSON.stringify(s));
  } catch {
    // Private mode or storage blocked: the session lives in memory only.
  }
  listeners.forEach((l) => l());
}

export function getSession(): Session {
  return current;
}

export function setSession(patch: Session) {
  write({ ...current, ...patch });
}

export function clearToken() {
  const { token: _t, console: _c, issued_at: _i, expires_at: _e, ...rest } = current;
  void _t, void _c, void _i, void _e;
  write(rest);
}

/** Keep a console session: `{token, expires_at}` from a sign-in, exchange or refresh. */
export function setConsoleToken(token: string, expiresAt: number) {
  setSession({ token, console: true, issued_at: Math.floor(Date.now() / 1000), expires_at: expiresAt });
}

// ── console sign-in binding (docs/CONTROL_PLANE.md section 7.2) ─────────────
// 32 random bytes stay in this tab's sessionStorage; the server sees only their
// SHA-256 when sign-in starts, and the bytes themselves at the exchange, so a
// callback link completed in another browser cannot be exchanged here.

const LOGIN = "onto.login";

function b64url(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** A new binding for a sign-in start: keeps the bytes, returns base64url(SHA-256(bytes)). */
export async function newLoginBinding(): Promise<string> {
  const raw = crypto.getRandomValues(new Uint8Array(32));
  try {
    sessionStorage.setItem(LOGIN, b64url(raw));
  } catch {
    // Without storage the exchange cannot prove this tab started the sign-in.
  }
  const digest = await crypto.subtle.digest("SHA-256", raw);
  return b64url(new Uint8Array(digest));
}

/** The binding of the sign-in this tab started (base64url of the bytes); used once. */
export function takeLoginBinding(): string | null {
  try {
    const v = sessionStorage.getItem(LOGIN);
    sessionStorage.removeItem(LOGIN);
    return v;
  } catch {
    return null;
  }
}

// ── console session refresh ──────────────────────────────────────────────────

let timer: number | undefined;

async function renew() {
  const s = current;
  if (!s.console || !s.token) return;
  try {
    const res = await fetch("/v1/console/auth/refresh", { method: "POST", headers: { authorization: `Bearer ${s.token}` } });
    if (res.status === 401) {
      clearToken();
      return;
    }
    if (!res.ok) throw new Error(res.statusText);
    const body = (await res.json()) as { token: string; expires_at: number };
    if (current.token === s.token) setConsoleToken(body.token, body.expires_at);
  } catch {
    // Offline or a server error: try again in 30 s while the token is still valid.
    if (current.token === s.token) schedule(30_000);
  }
}

function schedule(delay?: number) {
  window.clearTimeout(timer);
  const s = current;
  if (!s.console || !s.token || !s.expires_at) return;
  const issued = s.issued_at ?? Math.floor(Date.now() / 1000);
  // Renew once less than a quarter of the lifetime is left.
  const at = (s.expires_at - (s.expires_at - issued) / 4) * 1000;
  timer = window.setTimeout(() => void renew(), delay ?? Math.max(0, at - Date.now()));
}

/** Renew the console token while the tab is open; returns the cleanup. */
export function startSessionRefresh(): () => void {
  let token = current.token;
  schedule();
  const onChange = () => {
    if (current.token !== token) {
      token = current.token;
      schedule();
    }
  };
  // Timers sleep with the tab; check again when it is shown.
  const onVisible = () => document.visibilityState === "visible" && schedule();
  listeners.add(onChange);
  document.addEventListener("visibilitychange", onVisible);
  return () => {
    listeners.delete(onChange);
    document.removeEventListener("visibilitychange", onVisible);
    window.clearTimeout(timer);
  };
}

export function useSession(): Session {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => current,
  );
}

/** Pick up credentials from the address, then strip the token from it. */
export function absorbLocation() {
  const hash = window.location.hash;
  const q = hash.indexOf("?");
  if (q < 0) return;
  const params = new URLSearchParams(hash.slice(q + 1));
  const patch: Session = {};
  for (const k of ["token", "tenant", "workspace"] as const) {
    const v = params.get(k);
    if (v) patch[k] = v;
  }
  // A token from a link is an operator or backend-minted session, not a console one.
  if (patch.token) patch.console = false;
  if (Object.keys(patch).length) setSession(patch);
  if (params.has("token")) {
    params.delete("token");
    const rest = params.toString();
    const clean = hash.slice(0, q) + (rest ? `?${rest}` : "");
    window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}${clean}`);
  }
}

/** Accept a session from an embedding page on an allowed origin. */
export function listenForHost(allowedOrigins: string[], onFocus?: (id: string) => void) {
  const allowed = new Set([window.location.origin, ...allowedOrigins]);
  const onMessage = (ev: MessageEvent) => {
    if (!allowed.has(ev.origin) || typeof ev.data !== "object" || ev.data === null) return;
    const d = ev.data as Record<string, unknown>;
    if (d.type === "onto:session") {
      const patch: Session = {};
      if (typeof d.token === "string") {
        patch.token = d.token;
        patch.console = false;
      }
      if (typeof d.tenant === "string") patch.tenant = d.tenant;
      if (typeof d.workspace === "string") patch.workspace = d.workspace;
      setSession(patch);
    } else if (d.type === "onto:focus" && typeof d.id === "string") {
      onFocus?.(d.id);
    }
  };
  window.addEventListener("message", onMessage);
  if (window.parent !== window) {
    // Tell the host we are ready for a session; it answers with postMessage.
    for (const origin of allowedOrigins) {
      try {
        window.parent.postMessage({ type: "onto:ready" }, origin);
      } catch {
        // The origin is not the parent's; ignore.
      }
    }
  }
  return () => window.removeEventListener("message", onMessage);
}
