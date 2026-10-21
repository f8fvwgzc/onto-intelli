import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import "./styles.css";
import { Shell } from "./components/Shell";
import { Spinner } from "./components/ui";
import { ApiError, errorText, uiConfig, type UiConfig } from "./lib/api";
import { useWhoami } from "./lib/queries";
import { navigate, useRoute } from "./lib/router";
import { absorbLocation, listenForHost } from "./lib/session";
import { BuildPage } from "./pages/BuildPage";
import { EntitiesPage } from "./pages/EntitiesPage";
import { ExplorePage } from "./pages/ExplorePage";
import { MapPage } from "./pages/MapPage";
import { ProposalsPage } from "./pages/ProposalsPage";
import { SchemaPage } from "./pages/SchemaPage";
import { SignIn } from "./pages/SignIn";

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

function App() {
  const [config, setConfig] = useState<UiConfig | null>(null);
  const route = useRoute();
  const me = useWhoami();

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

  const title = me.data?.ui.title ?? config?.title ?? "onto-intelli";
  if (me.isLoading) {
    return (
      <div className="grid h-dvh place-items-center">
        <Spinner label="Connecting" />
      </div>
    );
  }
  if (me.error) {
    const unauth = me.error instanceof ApiError && me.error.status === 401;
    return <SignIn title={title} embedded={embedded} error={unauth ? undefined : errorText(me.error)} />;
  }

  const page = route.path[0];
  return (
    <Shell title={title} embedded={embedded}>
      {page === "schema" ? (
        <SchemaPage />
      ) : page === "proposals" ? (
        <ProposalsPage />
      ) : page === "entities" ? (
        <EntitiesPage />
      ) : page === "explore" ? (
        <ExplorePage />
      ) : page === "build" ? (
        <BuildPage />
      ) : (
        <MapPage />
      )}
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
