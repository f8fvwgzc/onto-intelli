import { StrictMode, Suspense, lazy, useEffect, useState, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import "./styles.css";
import { selectProject } from "./components/ProjectSwitcher";
import { Shell } from "./components/Shell";
import { Button, EmptyState, ErrorState, Spinner } from "./components/ui";
import { ApiError, errorText, uiConfig, type UiConfig } from "./lib/api";
import { useMe, useProviders, type Providers } from "./lib/console";
import { useWhoami } from "./lib/queries";
import { href, navigate, replaceRoute, useRoute } from "./lib/router";
import { absorbLocation, listenForHost, startSessionRefresh, useSession } from "./lib/session";
import { BuildPage } from "./pages/BuildPage";
import { EntitiesPage } from "./pages/EntitiesPage";
import { ExplorePage } from "./pages/ExplorePage";
import { MapPage } from "./pages/MapPage";
import { ProposalsPage } from "./pages/ProposalsPage";
import { SchemaPage } from "./pages/SchemaPage";
import { SignIn } from "./pages/SignIn";
import { AuthCallback, ConsoleSignIn } from "./pages/console/SignInPage";
import { InvitePage } from "./pages/console/InvitePage";

// Console pages load on first use: operators who never sign in to the console
// do not download them.
const ProjectsPage = lazy(() => import("./pages/console/ProjectsPage"));
const OverviewPage = lazy(() => import("./pages/console/OverviewPage"));
const TracesPage = lazy(() => import("./pages/console/TracesPage"));
const TracePage = lazy(() => import("./pages/console/TracePage"));
const SessionsPage = lazy(() => import("./pages/console/SessionsPage"));
const UsagePage = lazy(() => import("./pages/console/UsagePage"));
const ConnectPage = lazy(() => import("./pages/console/ConnectPage"));
const KeysPage = lazy(() => import("./pages/console/KeysPage"));
const LlmPage = lazy(() => import("./pages/console/LlmPage"));
const MembersPage = lazy(() => import("./pages/console/MembersPage"));
const SettingsPage = lazy(() => import("./pages/console/SettingsPage"));
// The assurance area (packs, reasoning, shapes, lint) loads on first visit.
const AssurePage = lazy(() => import("./pages/assure/AssurePage"));
const AutomatePage = lazy(() => import("./pages/automate/AutomatePage"));

absorbLocation();
const embedded = window.parent !== window;

const queries = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 10_000,
      refetchOnWindowFocus: false,
      retry: (n, e) => !(e instanceof ApiError && e.status < 500) && n < 2,
    },
  },
});

function Center({ children }: { children: ReactNode }) {
  return <div className="grid h-dvh place-items-center px-4">{children}</div>;
}

function App() {
  const [config, setConfig] = useState<UiConfig | null>(null);
  const route = useRoute();
  const session = useSession();
  const providers = useProviders(!embedded);

  useEffect(() => {
    void uiConfig().then((c) => {
      setConfig(c);
      document.title = c.title;
    });
  }, []);
  useEffect(() => {
    if (!config) return;
    return listenForHost(config.embed_origins, (id) => navigate(["map"], { focus: id }));
  }, [config]);

  const title = config?.title ?? "onto-intelli";
  if (!embedded && providers.isLoading) {
    return (
      <Center>
        <Spinner label="Connecting" />
      </Center>
    );
  }
  // The console is on when the server answers its providers route. Embedded
  // views, link tokens and operator tokens keep the operator flow.
  const consoleOn = !embedded ? providers.data : null;
  if (consoleOn) {
    const page = route.path[0];
    if (page === "auth" && route.path[1] === "callback") return <AuthCallback title={title} />;
    if (page === "invite") return <InvitePage title={title} providers={consoleOn} />;
    const operatorForm = page === "signin" && route.path[1] === "token";
    if (!operatorForm && (session.console || !session.token)) return <ConsoleRoot title={title} providers={consoleOn} />;
  }
  return <OperatorApp title={title} consoleOn={Boolean(consoleOn)} />;
}

/** The operator flow: a config token, a minted session, an embedding page or dev mode. */
function OperatorApp({ title: fallback, consoleOn }: { title: string; consoleOn: boolean }) {
  const route = useRoute();
  const me = useWhoami();
  const title = me.data?.ui.title ?? fallback;
  if (me.isLoading) {
    return (
      <Center>
        <Spinner label="Connecting" />
      </Center>
    );
  }
  if (me.error || (route.path[0] === "signin" && route.path[1] === "token")) {
    const unauth = me.error instanceof ApiError && me.error.status === 401;
    return <SignIn title={title} embedded={embedded} error={me.error && !unauth ? errorText(me.error) : undefined} consoleLink={consoleOn} />;
  }
  return (
    <Shell title={title} embedded={embedded}>
      <OntologyPage page={route.path[0]} />
    </Shell>
  );
}

function OntologyPage({ page }: { page: string }) {
  return page === "schema" ? (
    <SchemaPage />
  ) : page === "proposals" ? (
    <ProposalsPage />
  ) : page === "entities" ? (
    <EntitiesPage />
  ) : page === "explore" ? (
    <ExplorePage />
  ) : page === "build" ? (
    <BuildPage />
  ) : page === "assure" ? (
    <Suspense
      fallback={
        <div className="p-5">
          <Spinner />
        </div>
      }
    >
      <AssurePage />
    </Suspense>
  ) : page === "automate" ? (
    <Suspense
      fallback={
        <div className="p-5">
          <Spinner />
        </div>
      }
    >
      <AutomatePage />
    </Suspense>
  ) : (
    <MapPage />
  );
}

const ONTOLOGY = new Set(["map", "explore", "entities", "schema", "build", "proposals", "assure", "automate"]);
const NEEDS_PROJECT = new Set(["overview", "traces", "sessions", "usage", "connect", "keys", "llm", "settings", ...ONTOLOGY]);

/** The console: sign-in until `/v1/console/me` answers, then the shell with the project pages. */
function ConsoleRoot({ title, providers }: { title: string; providers: Providers }) {
  const session = useSession();
  const me = useMe();
  const route = useRoute();

  useEffect(() => startSessionRefresh(), []);

  // Keep a valid project selected: the session's, else the first of the organization, else any.
  useEffect(() => {
    const m = me.data;
    if (!m) return;
    if (m.projects.some((p) => p.ref === session.tenant)) return;
    const pick = m.projects.find((p) => p.org_id === session.org) ?? m.projects[0];
    if (pick) selectProject(pick);
  }, [me.data, session.tenant, session.org]);

  // The console opens on the overview.
  useEffect(() => {
    if (!me.data) return;
    const h = window.location.hash;
    if (!h || h === "#" || h === "#/" || h.startsWith("#/signin")) replaceRoute(["overview"]);
  }, [me.data]);

  if (!session.token || (me.error instanceof ApiError && me.error.status === 401)) {
    return <ConsoleSignIn title={title} providers={providers} />;
  }
  if (me.isLoading || !me.data) {
    return (
      <Center>
        {me.error ? (
          <div className="w-full max-w-md">
            <ErrorState error={me.error} retry={() => void me.refetch()} />
          </div>
        ) : (
          <Spinner label="Opening the console" />
        )}
      </Center>
    );
  }

  // An address without a route opens the overview (the operator flow opens the map).
  const bare = /^#?\/?$/.test(window.location.hash);
  const page = bare ? "overview" : (route.path[0] ?? "overview");
  const hasProject = me.data.projects.some((p) => p.ref === session.tenant);
  if (!hasProject && me.data.projects.length) {
    // The effect above is selecting a project; requests with a stale tenant would be refused.
    return (
      <Center>
        <Spinner label="Opening the project" />
      </Center>
    );
  }
  let body: ReactNode;
  if (NEEDS_PROJECT.has(page) && !hasProject) {
    body = (
      <EmptyState title="No project yet" action={<Button variant="primary" onClick={() => navigate(["projects"], { new: "1" })}>New project</Button>}>
        Projects hold an ontology, its API keys, traces and LLM settings. {me.data.orgs.length ? "Create one to start." : "You are not a member of an organization yet; ask for an invitation."}
      </EmptyState>
    );
  } else if (ONTOLOGY.has(page)) {
    body = <OntologyPage page={page} />;
  } else {
    body = (
      <Suspense
        fallback={
          <div className="p-5">
            <Spinner />
          </div>
        }
      >
        {page === "projects" ? (
          <ProjectsPage />
        ) : page === "traces" && route.path[1] ? (
          <TracePage id={route.path[1]} />
        ) : page === "traces" ? (
          <TracesPage />
        ) : page === "sessions" ? (
          <SessionsPage id={route.path[1]} />
        ) : page === "usage" ? (
          <UsagePage />
        ) : page === "connect" ? (
          <ConnectPage />
        ) : page === "keys" ? (
          <KeysPage />
        ) : page === "llm" ? (
          <LlmPage />
        ) : page === "members" ? (
          <MembersPage />
        ) : page === "settings" ? (
          <SettingsPage />
        ) : page === "overview" ? (
          <OverviewPage />
        ) : (
          <EmptyState title="Page not found" action={<a className="text-sm text-accent hover:underline" href={href(["overview"])}>Go to the overview</a>} />
        )}
      </Suspense>
    );
  }
  return (
    <Shell title={title} embedded={false} consoleMode>
      {body}
    </Shell>
  );
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queries}>
      <App />
    </QueryClientProvider>
  </StrictMode>,
);
