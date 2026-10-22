import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";

import { Logo } from "../components/Shell";
import { Button, Field, Input } from "../components/ui";
import { href, navigate, useRoute } from "../lib/router";
import { setSession, useSession } from "../lib/session";

export function SignIn({ title, error, embedded, consoleLink }: { title: string; error?: string; embedded: boolean; consoleLink?: boolean }) {
  const s = useSession();
  const route = useRoute();
  const [token, setToken] = useState("");
  const [tenant, setTenant] = useState(s.tenant ?? "");
  const [workspace, setWorkspace] = useState(s.workspace ?? "");
  const qc = useQueryClient();

  if (embedded) {
    return (
      <div className="grid h-dvh place-items-center bg-page px-6 text-center">
        <div className="max-w-sm">
          <Logo />
          <p className="mt-3 font-medium">Waiting for a session</p>
          <p className="mt-1 text-sm text-ink-2">The page embedding this view hands over a session. If nothing happens, reload it.</p>
          {error ? <p className="mt-3 text-sm text-danger">{error}</p> : null}
        </div>
      </div>
    );
  }

  return (
    <div className="grid min-h-dvh place-items-center bg-sunken px-4 py-10">
      <form
        className="w-full max-w-[380px] rounded-modal bg-raised p-6 shadow-overlay"
        onSubmit={(e) => {
          e.preventDefault();
          setSession({ token: token.trim() || undefined, console: false, tenant: tenant.trim() || undefined, workspace: workspace.trim() || undefined });
          void qc.invalidateQueries();
          if (route.path[0] === "signin") navigate(["map"]);
        }}
      >
        <div className="flex items-center gap-2.5">
          <Logo />
          <span className="font-semibold tracking-tight">{title}</span>
        </div>
        <h1 className="mt-6 text-lg font-semibold tracking-tight">Open your workspace map</h1>
        <p className="mt-1 text-sm text-ink-2">
          Paste a token from your onto-intelli configuration, or a session minted with <code className="font-mono text-[13px]">POST /v1/auth/session</code>.
        </p>
        <div className="mt-5 flex flex-col gap-4">
          <Field label="Token" error={error}>
            {(id) => <Input id={id} type="password" autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)} required autoFocus />}
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Tenant" hint="Blank: the default">
              {(id) => <Input id={id} value={tenant} onChange={(e) => setTenant(e.target.value)} />}
            </Field>
            <Field label="Workspace" hint="Blank: the default">
              {(id) => <Input id={id} value={workspace} onChange={(e) => setWorkspace(e.target.value)} />}
            </Field>
          </div>
          <Button variant="primary" type="submit" className="w-full">
            Open map
          </Button>
        </div>
        <p className="mt-4 text-xs text-ink-3">The token stays in this tab and is sent only to this server.</p>
        {consoleLink ? (
          <p className="mt-2 text-xs text-ink-3">
            <a href={href(["signin"])} className="text-accent hover:underline">
              Sign in to the console instead
            </a>
          </p>
        ) : null}
      </form>
    </div>
  );
}
