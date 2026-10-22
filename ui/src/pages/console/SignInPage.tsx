// Console sign-in (docs/CONTROL_PLANE.md section 7): IdP buttons (OpenID
// Connect with PKCE; the tab keeps a binding), the bootstrap admin token, and
// automatic sign-in in dev mode. The operator-token form stays one link away.

import { useEffect, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { KeyRound, LogIn } from "lucide-react";

import { Logo } from "../../components/Shell";
import { Button, Field, Input, Spinner } from "../../components/ui";
import { ApiError, errorText } from "../../lib/api";
import { consoleApi, type Providers, type SignedIn } from "../../lib/console";
import { href, navigate } from "../../lib/router";
import { newLoginBinding, setConsoleToken, setSession, takeLoginBinding } from "../../lib/session";

const SIGNED_OUT = "onto.signedout";

/** Remember an explicit sign-out, so dev mode does not sign straight back in. */
export function markSignedOut(on: boolean) {
  try {
    if (on) sessionStorage.setItem(SIGNED_OUT, "1");
    else sessionStorage.removeItem(SIGNED_OUT);
  } catch {
    // Storage blocked: dev mode signs in again, which is harmless.
  }
}

function signedOut(): boolean {
  try {
    return sessionStorage.getItem(SIGNED_OUT) === "1";
  } catch {
    return false;
  }
}

/** Keep a sign-in's session and select its organization. */
export function useAcceptSignIn() {
  const qc = useQueryClient();
  return (s: SignedIn) => {
    markSignedOut(false);
    setConsoleToken(s.token, s.expires_at);
    if (s.org) setSession({ org: s.org.id });
    void qc.invalidateQueries();
  };
}

/** Where to come back to after the IdP: the console route the tab is on, else the overview. */
function returnTo(): string {
  const h = window.location.hash;
  const ok = /^#\/(overview|traces|sessions|usage|connect|keys|llm|members|settings|projects|map|explore|entities|schema|build|proposals)\b/.test(h);
  return ok && h.length <= 512 && !/\s/.test(h) ? h : "#/overview";
}

export function ProviderButtons({ providers, invite }: { providers: Providers["providers"]; invite?: string }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string>();
  return (
    <div className="flex flex-col gap-2">
      {providers.map((p) => (
        <Button
          key={p.id}
          variant="secondary"
          className="w-full"
          busy={busy === p.id}
          icon={<LogIn className="size-4" aria-hidden />}
          onClick={async () => {
            setBusy(p.id);
            setError(undefined);
            try {
              const binding = await newLoginBinding();
              window.location.assign(consoleApi.startUrl(p.id, binding, invite ? "#/overview" : returnTo(), invite));
            } catch (e) {
              setBusy(null);
              setError(`Sign-in needs a secure page (https or localhost): ${errorText(e)}`);
            }
          }}
        >
          Continue with {p.name}
        </Button>
      ))}
      {error ? <p className="text-xs text-danger">{error}</p> : null}
    </div>
  );
}

export function AuthFrame({ title, children, footer }: { title: string; children: ReactNode; footer?: ReactNode }) {
  return (
    <div className="grid min-h-dvh place-items-center bg-sunken px-4 py-10">
      <div className="w-full max-w-[380px]">
        <main className="rounded-modal bg-raised p-6 shadow-overlay">
          <div className="flex items-center gap-2.5">
            <Logo />
            <span className="font-semibold tracking-tight">{title}</span>
          </div>
          {children}
        </main>
        {footer ? <div className="mt-4 text-center text-xs text-ink-3">{footer}</div> : null}
      </div>
    </div>
  );
}

let devTried = false;

export function ConsoleSignIn({ title, providers, error: initialError }: { title: string; providers: Providers; error?: string }) {
  const accept = useAcceptSignIn();
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState<"dev" | "token" | null>(null);
  const [error, setError] = useState(initialError);

  const dev = async () => {
    setBusy("dev");
    setError(undefined);
    try {
      accept(await consoleApi.dev());
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  useEffect(() => {
    if (providers.dev_mode && !signedOut() && !devTried) {
      devTried = true;
      void dev();
    }
    // Once per page load.
  }, [providers.dev_mode]);

  const none = !providers.providers.length && !providers.bootstrap && !providers.dev_mode;

  return (
    <AuthFrame
      title={title}
      footer={
        <a href={href(["signin", "token"])} className="underline-offset-2 hover:text-ink hover:underline">
          Use an operator token instead
        </a>
      }
    >
      <h1 className="mt-6 text-lg font-semibold tracking-tight">Sign in to the console</h1>
      <p className="mt-1 text-sm text-ink-2">Projects, API keys, traces and your LLM settings.</p>
      <div className="mt-5 flex flex-col gap-4">
        {busy === "dev" ? <Spinner label="Signing in as the developer" /> : null}
        {providers.providers.length ? <ProviderButtons providers={providers.providers} /> : null}
        {providers.dev_mode && busy !== "dev" ? (
          <Button variant="primary" className="w-full" onClick={() => void dev()}>
            Continue as developer
          </Button>
        ) : null}
        {providers.bootstrap ? (
          <form
            className="flex flex-col gap-3"
            onSubmit={async (e) => {
              e.preventDefault();
              setBusy("token");
              setError(undefined);
              try {
                accept(await consoleApi.bootstrap(token.trim()));
                setToken("");
              } catch (err) {
                setError(errorText(err));
              } finally {
                setBusy(null);
              }
            }}
          >
            {providers.providers.length || providers.dev_mode ? <p className="text-xs text-ink-3">or with the admin token</p> : null}
            <Field label="Admin token" hint="The value of ONTO_ADMIN_TOKEN on the server">
              {(id) => <Input id={id} type="password" autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)} required />}
            </Field>
            <Button type="submit" variant={providers.providers.length ? "secondary" : "primary"} busy={busy === "token"} icon={<KeyRound className="size-4" aria-hidden />}>
              Sign in with admin token
            </Button>
          </form>
        ) : null}
        {none ? <p className="text-sm text-ink-2">This server offers no console sign-in. Ask its operator for an operator token.</p> : null}
        {error ? (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        ) : null}
      </div>
    </AuthFrame>
  );
}

const CALLBACK_ERRORS: Record<string, string> = {
  invalid_state: "That sign-in link expired or was already used. Start again.",
  invalid_request: "The identity provider sent no authorization code.",
  idp_unavailable: "The identity provider could not be reached. Try again in a moment.",
  token_rejected: "The identity provider's answer did not pass verification.",
  signup_closed: "Sign-up is closed on this server. Ask an admin for an invitation.",
  domain_not_allowed: "Your email domain may not sign up here.",
  user_disabled: "This account is disabled.",
  access_denied: "Sign-in was cancelled.",
  server_error: "The server could not complete the sign-in.",
};

let exchanged = "";

/** `#/auth/callback?ticket=…&return_to=…` or `?error=…`: exchange the ticket and go on. */
export function AuthCallback({ title }: { title: string }) {
  const accept = useAcceptSignIn();
  // Read once, then remove the ticket from the address at once.
  const [params] = useState(() => {
    const raw = window.location.hash;
    const q = new URLSearchParams(raw.slice(raw.indexOf("?") + 1));
    if (raw.includes("?")) window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}#/auth/callback`);
    return { ticket: q.get("ticket"), error: q.get("error"), description: q.get("error_description"), returnTo: q.get("return_to") };
  });
  const [error, setError] = useState<string | undefined>(() =>
    params.error ? (CALLBACK_ERRORS[params.error] ?? params.description ?? params.error) : params.ticket ? undefined : "This page needs a sign-in ticket. Start the sign-in again.",
  );

  useEffect(() => {
    const ticket = params.ticket;
    if (!ticket || params.error || exchanged === ticket) return;
    exchanged = ticket;
    const binding = takeLoginBinding();
    if (!binding) {
      setError("This sign-in was started in another tab or browser. Start it again here.");
      return;
    }
    consoleApi
      .exchange(ticket, binding)
      .then((s) => {
        accept(s);
        const to = params.returnTo && params.returnTo.startsWith("#/") ? params.returnTo : "#/overview";
        window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}${to}`);
        window.dispatchEvent(new HashChangeEvent("hashchange"));
      })
      .catch((e) =>
        setError(
          e instanceof ApiError && e.status === 401 ? "That sign-in could not be completed (the link was used, expired, or belongs to another tab). Start it again." : errorText(e),
        ),
      );
  }, []);

  return (
    <AuthFrame title={title}>
      {error ? (
        <>
          <h1 className="mt-6 text-lg font-semibold tracking-tight">Sign-in did not complete</h1>
          <p role="alert" className="mt-2 text-sm text-ink-2">
            {error}
          </p>
          <Button variant="primary" className="mt-5 w-full" onClick={() => navigate(["signin"])}>
            Back to sign-in
          </Button>
        </>
      ) : (
        <div className="mt-6">
          <Spinner label="Signing you in" />
        </div>
      )}
    </AuthFrame>
  );
}
