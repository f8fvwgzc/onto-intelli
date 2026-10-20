//! Rule-based intent classification: microseconds, no model call.
//! Uses the schema's glossary and action-type phrases, so new vocabulary and
//! new commands need no code changes.

use serde::{Deserialize, Serialize};

use onto_core::Schema;
use onto_core::metrics::Measure;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Lookup,
    Owner,
    Assignee,
    Status,
    Members,
    Count,
    List,
    Blockers,
    Dependencies,
    Impact,
    Relation,
    Workload,
    History,
    Summarize,
    Reasoning,
    /// An imperative that maps to an action type ("assign issue 433 to Aiko").
    Command,
    /// "Who knows about payments?"
    Expert,
    /// Governed KPI: lead time, throughput, failure rate, grouped counts.
    Metric,
    /// "What changed this week?"
    Changes,
    /// Data quality: stale, orphaned, duplicate, missing links.
    Quality,
    /// "What if task 31 were done?"
    WhatIf,
    /// "Which table holds revenue?"
    DataLocation,
    /// "How do orders join to customers?"
    JoinPath,
    /// "Which revenue metric is the trusted one?"
    TrustedMetric,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Lookup => "lookup",
            Intent::Owner => "owner",
            Intent::Assignee => "assignee",
            Intent::Status => "status",
            Intent::Members => "members",
            Intent::Count => "count",
            Intent::List => "list",
            Intent::Blockers => "blockers",
            Intent::Dependencies => "dependencies",
            Intent::Impact => "impact",
            Intent::Relation => "relation",
            Intent::Workload => "workload",
            Intent::History => "history",
            Intent::Summarize => "summarize",
            Intent::Reasoning => "reasoning",
            Intent::Command => "command",
            Intent::Expert => "expert",
            Intent::Metric => "metric",
            Intent::Changes => "changes",
            Intent::Quality => "quality",
            Intent::WhatIf => "what_if",
            Intent::DataLocation => "data_location",
            Intent::JoinPath => "join_path",
            Intent::TrustedMetric => "trusted_metric",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateFilter {
    Open,
    Closed,
    Is(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Classified {
    pub intent: Intent,
    /// "why"/"explain" questions need reasoning even when the graph has the facts.
    pub why: bool,
    pub kind: Option<String>,
    pub state: Option<StateFilter>,
    /// Refers to the asker ("my issues", "what am I working on").
    pub mine: bool,
    /// "per team", "by priority", "broken down by assignee".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    /// Time window in days ("this week" = 7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<Measure>,
    /// Action type for commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Hypothetical state for what-if questions ("done", "resolved").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whatif_state: Option<String>,
}

fn has_any(q: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| q.contains(n))
}

const POLITE: &[&str] =
    &["please ", "pls ", "can you ", "could you ", "would you ", "kindly ", "go ahead and ", "now "];
const DIMENSION_WORDS: &[&str] = &[
    "team",
    "teams",
    "priority",
    "assignee",
    "assignees",
    "state",
    "status",
    "kind",
    "type",
    "project",
    "projects",
    "owner",
    "owners",
    "person",
    "people",
    "day",
    "days",
    "actor",
    "action",
    "reporter",
];
const NOT_GROUP_BY: &[&str] = &[
    "owned",
    "blocked",
    "assigned",
    "reported",
    "requested",
    "generated",
    "managed",
    "affected",
    "required",
    "caused",
    "used",
];

/// Which action type an imperative starts with (longest phrase wins).
pub fn command_action(question: &str, schema: &Schema) -> Option<String> {
    let mut q = format!("{} ", question.trim().to_lowercase().trim_end_matches(['?', '!', '.']));
    loop {
        let before = q.clone();
        for p in POLITE {
            if let Some(rest) = q.strip_prefix(p) {
                q = rest.to_string();
            }
        }
        if q == before {
            break;
        }
    }
    let mut best: Option<(usize, String)> = None;
    for (name, at) in &schema.action_types {
        let mut phrases: Vec<String> = at.phrases.iter().map(|p| p.to_lowercase()).collect();
        phrases.push(name.replace('_', " "));
        for ph in phrases {
            if q.starts_with(&format!("{ph} ")) && best.as_ref().is_none_or(|(len, _)| ph.len() > *len) {
                best = Some((ph.len(), name.clone()));
            }
        }
    }
    best.map(|(_, n)| n)
}

/// "this week" → 7, "last 3 days" → 3, "since yesterday" → 2.
pub fn time_window(q: &str) -> Option<u32> {
    let words: Vec<&str> = q.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if matches!(*w, "last" | "past")
            && let Some(n) = words.get(i + 1).and_then(|n| n.parse::<u32>().ok())
        {
            let unit = words.get(i + 2).copied().unwrap_or("days");
            return Some(match unit.trim_end_matches('s') {
                "week" => n * 7,
                "month" => n * 30,
                "hour" => 1,
                _ => n,
            });
        }
    }
    if has_any(q, &[" today "]) {
        Some(1)
    } else if has_any(q, &[" yesterday "]) {
        Some(2)
    } else if has_any(q, &[" this week ", " last week ", " past week ", " recently ", " lately ", " seven days "]) {
        Some(7)
    } else if has_any(q, &[" this month ", " last month ", " past month "]) {
        Some(30)
    } else if has_any(q, &[" this quarter ", " last quarter ", " past quarter "]) {
        Some(90)
    } else if has_any(q, &[" this year ", " last year ", " past year "]) {
        Some(365)
    } else {
        None
    }
}

fn group_by(q: &str, words: &[&str], allow_by: bool) -> Option<(usize, String)> {
    for (i, w) in words.iter().enumerate() {
        let next = words.get(i + 1).copied();
        let explicit = matches!(*w, "per" | "each")
            || (*w == "by" && i > 0 && matches!(words[i - 1], "grouped" | "broken" | "breakdown" | "split" | "sliced"));
        let plain_by = *w == "by" && allow_by && i > 0 && !NOT_GROUP_BY.contains(&words[i - 1]);
        if (explicit || plain_by)
            && let Some(dim) = next.filter(|d| DIMENSION_WORDS.contains(d))
        {
            let canonical = match dim.trim_end_matches('s') {
                "status" | "statu" => "state",
                "type" => "kind",
                "person" | "people" | "peopl" => "assignee",
                d => d,
            };
            return Some((i + 1, canonical.to_string()));
        }
    }
    let _ = q;
    None
}

pub fn classify(question: &str, schema: &Schema) -> Classified {
    let q = format!(" {} ", question.to_lowercase().replace(['?', '!', ',', '.', ';', ':'], " "));
    let words: Vec<&str> = q.split_whitespace().collect();
    let why = has_any(&q, &[" why ", " explain ", " reason ", " reasons ", " cause ", " caused ", " root cause "]);
    let mine = words.iter().any(|w| matches!(*w, "my" | "me" | "i" | "mine" | "am"));
    let since_days = time_window(&q);
    let action = command_action(question, schema);
    let counting = has_any(&q, &[" how many ", " count ", " number of ", " total ", " how much "]);
    let listing = has_any(&q, &[" list ", " show all ", " show me all ", " which ", " all open ", " all the "]);
    let grouped = group_by(&q, &words, counting || listing);

    let measure = if has_any(
        &q,
        &[" lead time", " cycle time", " time to resolve", " time to close", " how long does it take", " how long do "],
    ) {
        Some(Measure::LeadTime)
    } else if has_any(
        &q,
        &[" failure rate", " fail rate", " success rate", " failed deployments", " deployment failures"],
    ) {
        Some(Measure::FailureRate)
    } else if has_any(&q, &[" throughput", " velocity"])
        || (counting
            && since_days.is_some()
            && has_any(&q, &[" completed", " finished", " closed", " resolved", " done ", " shipped"]))
    {
        Some(Measure::Throughput)
    } else if has_any(&q, &[" how active", " event volume", " activity volume", " most active"]) {
        Some(Measure::EventCount)
    } else {
        None
    };

    let whatif = has_any(&q, &[" what if ", " what happens if ", " suppose ", " hypothetically ", " if we "])
        || (words.first() == Some(&"if") && has_any(&q, &[" were ", " was ", " is ", " gets ", " becomes "]));

    let intent = if action.is_some() {
        Intent::Command
    } else if whatif {
        Intent::WhatIf
    } else if has_any(&q, &[" join ", " joins ", " joined ", " joining "])
        && has_any(&q, &[" to ", " with ", " and ", " on "])
        && !has_any(&q, &[" team ", " project ", " meeting ", " channel ", " company ", " call "])
    {
        Intent::JoinPath
    } else if has_any(&q, &[" metric", " kpi", " measure "])
        && has_any(
            &q,
            &[
                " trusted",
                " certified",
                " official",
                " canonical",
                " right one",
                " source of truth",
                " correct one",
                " should i use",
                " should we use",
            ],
        )
    {
        Intent::TrustedMetric
    } else if has_any(
        &q,
        &[
            " which table",
            " what table",
            " which view",
            " which dataset",
            " what dataset",
            " which column",
            " where is the data",
            " where do we store",
            " where do we keep",
            " which database",
        ],
    ) || (has_any(&q, &[" where is ", " where are ", " where can i find "])
        && has_any(&q, &[" stored", " kept", " data ", " table", " dataset", " recorded"]))
    {
        Intent::DataLocation
    } else if has_any(
        &q,
        &[
            " who knows",
            " expert",
            " who should i ask",
            " who can help",
            " who to ask",
            " go-to ",
            " knows about ",
            " knows the most",
            " most familiar",
        ],
    ) {
        Intent::Expert
    } else if has_any(
        &q,
        &[
            " data quality",
            " quality report",
            " stale ",
            " orphan",
            " duplicate",
            " dupes ",
            " inconsistent",
            " missing link",
            " data issues",
            " hygiene",
        ],
    ) {
        Intent::Quality
    } else if has_any(
        &q,
        &[
            " what changed",
            " what's changed",
            " what has changed",
            " recent changes",
            " what's new",
            " what is new",
            " changes since",
            " changed since",
            " updates since",
            " changelog",
            " what moved",
        ],
    ) {
        Intent::Changes
    } else if measure.is_some() {
        Intent::Metric
    } else if counting {
        Intent::Count
    } else if has_any(
        &q,
        &[
            " impact",
            " affected if ",
            " what breaks",
            " breaks if ",
            " blast radius",
            " downstream",
            " what depends on ",
            " who depends on ",
            " if it fails",
            " goes down",
            " fails",
        ],
    ) {
        Intent::Impact
    } else if has_any(&q, &[" block", " stuck", " waiting on ", " waiting for ", " held up"]) {
        Intent::Blockers
    } else if has_any(&q, &[" depend", " prerequisite", " requires ", " needs "]) {
        Intent::Dependencies
    } else if has_any(
        &q,
        &[" related to ", " connected to ", " relationship between ", " connection between ", " link between "],
    ) {
        Intent::Relation
    } else if has_any(
        &q,
        &[
            " history ",
            " what happened",
            " recent activity",
            " who changed",
            " timeline",
            " audit",
            " changes to ",
            " activity ",
        ],
    ) {
        Intent::History
    } else if has_any(&q, &[" members", " who is in ", " who are in ", " who works in ", " who's in ", " team of "]) {
        Intent::Members
    } else if has_any(&q, &[" own ", " owns ", " owner", " owned by ", " owning "]) {
        Intent::Owner
    } else if (has_any(&q, &[" what is ", " what are ", " what am "])
        && has_any(&q, &[" working on ", " assigned to "]))
        || has_any(&q, &[" workload", " busy", " plate", " my tasks ", " my issues "])
    {
        Intent::Workload
    } else if has_any(&q, &[" assigned", " assignee", " responsible", " working on ", " handling ", " who is on "]) {
        Intent::Assignee
    } else if has_any(&q, &[" status", " state of ", " progress", " is it done", " on track", " doing "]) {
        Intent::Status
    } else if listing {
        Intent::List
    } else if has_any(&q, &[" summarize", " summary", " overview", " brief me", " recap"]) {
        Intent::Summarize
    } else if !why
        && has_any(&q, &[" what is ", " who is ", " what's ", " who's ", " tell me about ", " describe ", " show "])
    {
        Intent::Lookup
    } else {
        Intent::Reasoning
    };

    let intent = match intent {
        // "Why ...?" is open-ended unless it is about blockers, impact or dependencies.
        i if why && !matches!(i, Intent::Blockers | Intent::Impact | Intent::Dependencies | Intent::Command) => {
            Intent::Reasoning
        }
        i => i,
    };

    // Kind: glossary-aware, bigrams first ("work items"), skipping the group-by word.
    let skip = grouped.as_ref().map(|(i, _)| *i);
    let mut kind = None;
    for i in 0..words.len() {
        if Some(i) == skip {
            continue;
        }
        let bigram = words.get(i + 1).map(|n| format!("{} {n}", words[i]));
        let found = bigram
            .as_deref()
            .and_then(|b| schema.canonical_kind(b).filter(|_| b.contains(' ')))
            .or_else(|| schema.canonical_kind(words[i]));
        if let Some(k) = found.filter(|k| k != "workspace") {
            kind = Some(k);
            if matches!(intent, Intent::Count | Intent::List | Intent::Metric) && words[i].ends_with('s') {
                break;
            }
        }
    }

    let state = if has_any(
        &q,
        &[" open ", " unresolved ", " active ", " pending ", " outstanding ", " remaining ", " in progress "],
    ) {
        Some(StateFilter::Open)
    } else if has_any(&q, &[" closed ", " done ", " resolved ", " completed ", " finished "])
        && measure != Some(Measure::Throughput)
    {
        Some(StateFilter::Closed)
    } else {
        kind.as_deref()
            .and_then(|k| schema.object_type(k))
            .and_then(|t| t.states.iter().find(|s| words.contains(&s.as_str())).cloned())
            .map(StateFilter::Is)
    };

    let whatif_state = whatif
        .then(|| {
            words
                .iter()
                .position(|w| matches!(*w, "were" | "was" | "is" | "gets" | "becomes" | "got" | "marked"))
                .and_then(|i| words.get(i + 1..))
                .and_then(|rest| {
                    rest.iter().find(|w| !matches!(**w, "already" | "now" | "finally" | "as" | "marked" | "set" | "to"))
                })
                .map(|w| w.to_string())
        })
        .flatten();

    Classified {
        intent,
        why,
        kind,
        state,
        mine,
        group_by: grouped.map(|(_, d)| d),
        since_days,
        measure,
        action,
        whatif_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(q: &str) -> Classified {
        classify(q, &Schema::enterprise())
    }

    #[test]
    fn classifies_common_questions() {
        assert_eq!(c("Who owns Project Alpha?").intent, Intent::Owner);
        assert_eq!(c("Who is assigned to issue #421?").intent, Intent::Assignee);
        assert_eq!(c("Who is responsible for issue #421?").intent, Intent::Assignee);
        let count = c("How many open issues does the Payments Team have?");
        assert_eq!(count.intent, Intent::Count);
        assert_eq!(count.kind.as_deref(), Some("issue"));
        assert_eq!(count.state, Some(StateFilter::Open));
        assert_eq!(c("What is blocking task 31?").intent, Intent::Blockers);
        let why = c("Why is Project Alpha blocked?");
        assert_eq!(why.intent, Intent::Blockers);
        assert!(why.why);
        assert_eq!(
            c("Why is issue #421 assigned to Kenji, and which team owns the affected service?").intent,
            Intent::Reasoning
        );
        assert_eq!(c("What breaks if PostgreSQL goes down?").intent, Intent::Impact);
        assert_eq!(c("How is Kenji related to PostgreSQL?").intent, Intent::Relation);
        assert_eq!(c("What is Kenji working on?").intent, Intent::Workload);
        assert!(c("what am I working on").mine);
        assert_eq!(c("What is Project Alpha?").intent, Intent::Lookup);
        assert_eq!(c("Who is in the Payments Team?").intent, Intent::Members);
        assert_eq!(c("What happened to deployment 182?").intent, Intent::History);
        assert_eq!(c("Should we delay the launch?").intent, Intent::Reasoning);
    }

    #[test]
    fn classifies_v02_questions() {
        let cmd = c("Please assign issue 433 to Aiko");
        assert_eq!(cmd.intent, Intent::Command);
        assert_eq!(cmd.action.as_deref(), Some("assign"));
        assert_eq!(c("mark task 32 as done").action.as_deref(), Some("set_state"));
        assert_eq!(c("create task \"Write docs\" in Project Alpha").action.as_deref(), Some("create_task"));
        assert_eq!(c("Who knows about PostgreSQL?").intent, Intent::Expert);
        let g = c("How many open issues per team?");
        assert_eq!(g.intent, Intent::Count);
        assert_eq!(g.group_by.as_deref(), Some("team"));
        assert_eq!(g.kind.as_deref(), Some("issue"));
        assert_eq!(c("count work items by priority").kind.as_deref(), Some("work_item"));
        assert_eq!(c("How many tickets are blocked by issue 1").group_by, None, "'blocked by' is not a group-by");
        let lt = c("What is the lead time for issues?");
        assert_eq!(lt.intent, Intent::Metric);
        assert_eq!(lt.measure, Some(Measure::LeadTime));
        let tp = c("How many tasks were completed this month?");
        assert_eq!(tp.measure, Some(Measure::Throughput));
        assert_eq!(tp.since_days, Some(30));
        let ch = c("What changed in Project Alpha this week?");
        assert_eq!(ch.intent, Intent::Changes);
        assert_eq!(ch.since_days, Some(7));
        assert_eq!(c("Show me stale issues").intent, Intent::Quality);
        let wi = c("What if task 31 were done?");
        assert_eq!(wi.intent, Intent::WhatIf);
        assert_eq!(wi.whatif_state.as_deref(), Some("done"));
        assert_eq!(time_window(" last 3 weeks "), Some(21));
    }

    #[test]
    fn classifies_catalog_questions() {
        assert_eq!(c("Which table holds revenue?").intent, Intent::DataLocation);
        assert_eq!(c("Where is customer churn stored?").intent, Intent::DataLocation);
        assert_eq!(c("How do orders join to customers?").intent, Intent::JoinPath);
        assert_eq!(c("How can I join order_items with customers").intent, Intent::JoinPath);
        assert_eq!(c("Which revenue metric is the trusted one?").intent, Intent::TrustedMetric);
        assert_ne!(c("When did Aiko join the team?").intent, Intent::JoinPath);
        assert_ne!(c("Where is Kenji?").intent, Intent::DataLocation);
        assert_eq!(c("What is the deployment failure rate?").intent, Intent::Metric);
    }
}
