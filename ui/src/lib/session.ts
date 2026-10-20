// Where the UI's credentials come from, in order:
// 1. a host page that embeds the UI posts {type: "onto:session", token, tenant, workspace}
//    (only from origins in [ui] embed_origins, or this origin);
// 2. a link with #…?token=…&tenant=…&workspace=… (the token is removed from the address at once);
// 3. the sign-in form (a token pasted by an operator).
// Kept in sessionStorage: it ends with the tab.

import { useSyncExternalStore } from "react";

export interface Session {
  token?: string;
  tenant?: string;
  workspace?: string;
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
  const { token: _drop, ...rest } = current;
  void _drop;
  write(rest);
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
      if (typeof d.token === "string") patch.token = d.token;
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
