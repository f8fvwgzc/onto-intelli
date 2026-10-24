// Assurance: industry packs, the reasoner and its certificates, shapes and
// schema lint. Each tab says what kind of answer it gives: a derived fact is
// the engine's opinion, a checked certificate is a proof under its rule
// table, a shape or lint report is a measurement.

import { Tabs } from "../../components/consoleUi";
import { PageHeader } from "../../components/Shell";
import { navigate, useRoute } from "../../lib/router";
import { LintTab } from "./LintTab";
import { PacksTab } from "./PacksTab";
import { ReasoningTab } from "./ReasoningTab";
import { ShapesTab } from "./ShapesTab";

type Tab = "packs" | "reasoning" | "shapes" | "lint";

const TABS: { value: Tab; label: string }[] = [
  { value: "packs", label: "Packs" },
  { value: "reasoning", label: "Reasoning" },
  { value: "shapes", label: "Shapes" },
  { value: "lint", label: "Lint" },
];

export default function AssurePage() {
  const route = useRoute();
  const tab = (TABS.some((t) => t.value === route.path[1]) ? route.path[1] : "packs") as Tab;
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader title="Assurance" context="Packs, inferences with certificates, shapes and lint: what is derived, measured or proved" />
      <div className="px-4 md:px-5">
        <Tabs<Tab> label="Assurance" value={tab} tabs={TABS} onChange={(t) => navigate(["assure", t], { workspace: route.query.get("workspace") ?? undefined })} />
      </div>
      {tab === "packs" ? <PacksTab id={route.path[2]} /> : tab === "reasoning" ? <ReasoningTab /> : tab === "shapes" ? <ShapesTab /> : <LintTab />}
    </div>
  );
}
