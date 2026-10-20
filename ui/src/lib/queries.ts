import { useQuery } from "@tanstack/react-query";

import { call, whoami } from "./api";
import { useSession } from "./session";
import type { Proposal, SchemaDoc, Whoami } from "./types";

/** Query keys carry the tenant and workspace so switching either refetches. */
export function useScope(): [string, string] {
  const s = useSession();
  return [s.tenant ?? "", s.workspace ?? ""];
}

export function useWhoami() {
  const s = useSession();
  return useQuery<Whoami>({ queryKey: ["whoami", s.token ?? "", s.tenant ?? ""], queryFn: whoami, retry: false });
}

export function useSchema() {
  const scope = useScope();
  return useQuery<SchemaDoc>({ queryKey: ["schema", ...scope], queryFn: () => call<SchemaDoc>("schema.get"), staleTime: 30_000 });
}

export function useOpenProposals() {
  const scope = useScope();
  return useQuery<{ proposals: Proposal[] }>({
    queryKey: ["proposals", "open", ...scope],
    queryFn: () => call("schema.proposals", { status: "open", limit: 200 }),
    refetchInterval: 30_000,
  });
}
