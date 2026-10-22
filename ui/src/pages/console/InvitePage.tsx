import { useQuery } from "@tanstack/react-query";

import { Spinner } from "../../components/ui";
import { ApiError, errorText } from "../../lib/api";
import { consoleApi, type Providers } from "../../lib/console";
import { when } from "../../lib/format";
import { href, useRoute } from "../../lib/router";
import { AuthFrame, ProviderButtons } from "./SignInPage";

/** `#/invite?code=…`: who invites you, then sign-in through an IdP, which accepts it. */
export function InvitePage({ title, providers }: { title: string; providers: Providers }) {
  const code = useRoute().query.get("code") ?? "";
  const inv = useQuery({ queryKey: ["console", "invitation", code], queryFn: () => consoleApi.invitation(code), enabled: Boolean(code), retry: false });

  return (
    <AuthFrame
      title={title}
      footer={
        <a href={href(["signin"])} className="hover:text-ink hover:underline">
          Sign in without accepting
        </a>
      }
    >
      {!code ? (
        <p className="mt-6 text-sm text-ink-2">This invitation link has no code. Ask for a new link.</p>
      ) : inv.isLoading ? (
        <div className="mt-6">
          <Spinner label="Opening the invitation" />
        </div>
      ) : inv.error ? (
        <>
          <h1 className="mt-6 text-lg font-semibold tracking-tight">
            {inv.error instanceof ApiError && inv.error.status === 404 ? "Invitation not found" : "The invitation did not load"}
          </h1>
          <p className="mt-1 text-sm text-ink-2">
            {inv.error instanceof ApiError && inv.error.status === 404 ? "It may have expired, been revoked or been used already. Ask for a new link." : errorText(inv.error)}
          </p>
        </>
      ) : inv.data ? (
        <>
          <h1 className="mt-6 text-lg font-semibold tracking-tight text-balance">Join {inv.data.org.name}</h1>
          <p className="mt-1 text-sm text-ink-2">
            You are invited as <span className="font-medium text-ink">{inv.data.role}</span>
            {inv.data.email ? <> ({inv.data.email})</> : null}. The invitation is valid until {when(inv.data.expires_at)}.
          </p>
          <div className="mt-5">
            {providers.providers.length ? (
              <>
                <p className="mb-2 text-[13px] font-medium text-ink">Sign in to accept</p>
                <ProviderButtons providers={providers.providers} invite={code} />
              </>
            ) : (
              <p className="text-sm text-ink-2">
                This server has no identity provider, so invitations cannot be accepted here. Ask its operator to add you with{" "}
                <code className="font-mono text-[13px]">onto console add-owner</code>, or to configure an IdP.
              </p>
            )}
          </div>
        </>
      ) : null}
    </AuthFrame>
  );
}
