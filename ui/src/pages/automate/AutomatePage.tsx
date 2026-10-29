// Automate: what the ontology does and sends out when it changes.
// Automations run governed action types and webhooks on matching changes;
// webhooks deliver signed CloudEvents to registered endpoints, each change
// enqueued once per endpoint and delivered at least once.

import { Tabs } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { navigate, useRoute } from "../../lib/router";
import { AutomationsTab } from "./AutomationsTab";
import { WebhooksTab } from "./WebhooksTab";

type Tab = "automations" | "webhooks";

const TABS: { value: Tab; label: string }[] = [
  { value: "automations", label: "Automations" },
  { value: "webhooks", label: "Webhooks" },
];

export default function AutomatePage() {
  const route = useRoute();
  const tab = (TABS.some((t) => t.value === route.path[1]) ? route.path[1] : "automations") as Tab;
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title="Automate" context="Runs and signed CloudEvents when the ontology changes" />
      <div className="px-4 md:px-5">
        <Tabs<Tab> label="Automate" value={tab} tabs={TABS} onChange={(t) => navigate(["automate", t], { workspace: route.query.get("workspace") ?? undefined })} />
      </div>
      {tab === "automations" ? <AutomationsTab name={route.path[2]} /> : <WebhooksTab name={route.path[2]} />}
    </div>
  );
}
