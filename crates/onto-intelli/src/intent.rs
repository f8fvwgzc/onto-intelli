//! Rule-based intent classification: microseconds, no model call.

use serde::{Deserialize, Serialize};

use onto_core::Schema;

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
}

fn has_any(q: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| q.contains(n))
}

pub fn classify(question: &str, schema: &Schema) -> Classified {
    let q = format!(" {} ", question.to_lowercase().replace(['?', '!', ',', '.'], " "));
    let words: Vec<&str> = q.split_whitespace().collect();
    let why = has_any(&q, &[" why ", " explain ", " reason ", " reasons ", " cause ", " caused ", " root cause "]);
    let mine = words.iter().any(|w| matches!(*w, "my" | "me" | "i" | "mine" | "am"));

    let intent = if has_any(&q, &[" how many ", " count ", " number of ", " total "]) {
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
    } else if has_any(&q, &[" list ", " show all ", " show me all ", " which ", " all open ", " all the "]) {
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
        // "Why ...?" is open-ended unless it is specifically about blockers.
        i if why && !matches!(i, Intent::Blockers | Intent::Impact | Intent::Dependencies) => Intent::Reasoning,
        i => i,
    };

    let mut kind = None;
    for w in &words {
        let singular = w
            .strip_suffix("es")
            .filter(|s| schema.object_type(s).is_some())
            .or_else(|| w.strip_suffix('s'))
            .unwrap_or(w);
        if schema.object_type(singular).is_some() && singular != "workspace" {
            kind = Some(singular.to_string());
            if matches!(intent, Intent::Count | Intent::List) && w.ends_with('s') {
                break;
            }
        }
    }

    let state = if has_any(
        &q,
        &[" open ", " unresolved ", " active ", " pending ", " outstanding ", " remaining ", " in progress "],
    ) {
        Some(StateFilter::Open)
    } else if has_any(&q, &[" closed ", " done ", " resolved ", " completed ", " finished "]) {
        Some(StateFilter::Closed)
    } else {
        kind.as_deref()
            .and_then(|k| schema.object_type(k))
            .and_then(|t| t.states.iter().find(|s| words.contains(&s.as_str())).cloned())
            .map(StateFilter::Is)
    };

    Classified { intent, why, kind, state, mine }
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
}
