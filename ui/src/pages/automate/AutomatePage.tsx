// Automate: what the ontology sends out when it changes. Webhooks deliver
// signed CloudEvents to registered endpoints; each change is enqueued once
// per endpoint and delivered at least once.

import { Tabs } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { navigate, useRoute } from "../../lib/router";
import { WebhooksTab } from "./WebhooksTab";

type Tab = "webhooks";

const TABS: { value: Tab; label: string }[] = [{ value: "webhooks", label: "Webhooks" }];

export default function AutomatePage() {
  const route = useRoute();
  const tab = (TABS.some((t) => t.value === route.path[1]) ? route.path[1] : "webhooks") as Tab;
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title="Automate" context="Signed CloudEvents to your receivers when the ontology changes" />
      {TABS.length > 1 ? (
        <div className="px-4 md:px-5">
          <Tabs<Tab> label="Automate" value={tab} tabs={TABS} onChange={(t) => navigate(["automate", t], { workspace: route.query.get("workspace") ?? undefined })} />
        </div>
      ) : null}
      <WebhooksTab name={route.path[2]} />
    </div>
  );
}
