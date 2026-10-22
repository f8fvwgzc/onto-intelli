//! Entity resolution: find the things a question mentions, without an LLM.
//!
//! Explicit ids (`issue:421`), hashtags (`#421`), exact names/aliases over
//! 1–5 word n-grams (longest first), then trigram fuzzy matching for typos.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use onto_core::graph::{Graph, normalize};
use onto_core::policy;
use onto_core::text::{is_stopword, similarity};
use onto_core::{Entity, Id, Ontology, Principal, Schema};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub id: Id,
    pub kind: String,
    pub title: String,
    /// The words in the question that matched.
    pub mention: String,
    pub confidence: f32,
    /// Word position of the mention (keeps question order).
    #[serde(default)]
    pub position: usize,
}

const MAX_NGRAM: usize = 5;
const FUZZY_THRESHOLD: f32 = 0.55;
const FUZZY_MAX_NAMES: usize = 50_000;

fn clean_word(w: &str) -> String {
    let w =
        w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '#' || c == ':' || c == '_' || c == '-' || c == '.'));
    let w = w.trim_end_matches('.');
    let w = w.strip_suffix("'s").or_else(|| w.strip_suffix("’s")).unwrap_or(w);
    w.to_lowercase()
}

pub fn resolve(onto: &Ontology, p: &Principal, ws: &str, question: &str) -> Vec<Resolved> {
    onto.read(|g, schema| resolve_in(g, schema, p, ws, question))
}

/// The question's words as positions in [`Resolved::position`] refer to them.
pub fn words(question: &str) -> Vec<String> {
    question
        .split(|c: char| {
            c.is_whitespace() || c == ',' || c == '?' || c == '!' || c == ';' || c == '(' || c == ')' || c == '"'
        })
        .map(clean_word)
        .filter(|w| !w.is_empty())
        .collect()
}

fn resolve_in(g: &Graph, schema: &Schema, p: &Principal, ws: &str, question: &str) -> Vec<Resolved> {
    let words = words(question);
    let usable = |e: &Entity| e.workspace == ws && policy::visible(p, e, g);
    let kind_words: Vec<&str> = words.iter().map(String::as_str).filter(|w| schema.object_type(w).is_some()).collect();
    let mut covered = vec![false; words.len()];
    let mut found: Vec<Resolved> = Vec::new();

    let push = |candidates: Vec<&Entity>,
                mention: String,
                position: usize,
                confidence: f32,
                found: &mut Vec<Resolved>|
     -> bool {
        let candidates: Vec<&Entity> = candidates.into_iter().filter(|e| usable(e)).collect();
        if candidates.is_empty() {
            return false;
        }
        // Prefer kinds named in the question ("issue #421" vs "task #421").
        let preferred: Vec<&&Entity> = candidates.iter().filter(|e| kind_words.contains(&e.kind.as_str())).collect();
        let (chosen, ambiguous): (Vec<&Entity>, bool) = match (preferred.len(), candidates.len()) {
            (1, _) => (vec![*preferred[0]], false),
            (_, 1) => (vec![candidates[0]], false),
            _ => (candidates.clone(), true),
        };
        for e in chosen {
            found.push(Resolved {
                id: e.id.clone(),
                kind: e.kind.clone(),
                title: e.label().to_string(),
                mention: mention.clone(),
                confidence: if ambiguous { confidence * 0.7 } else { confidence },
                position,
            });
        }
        true
    };

    // 1. Explicit ids.
    for (i, w) in words.iter().enumerate() {
        if w.contains(':')
            && let Some(e) = g.get(w)
            && push(vec![e], w.clone(), i, 1.0, &mut found)
        {
            covered[i] = true;
        }
    }

    // 2. Exact names, longest n-gram first.
    for n in (1..=MAX_NGRAM.min(words.len())).rev() {
        for start in 0..=words.len() - n {
            if covered[start..start + n].iter().any(|c| *c) {
                continue;
            }
            let gram: Vec<&str> = words[start..start + n].iter().map(String::as_str).collect();
            if n == 1 && (is_stopword(gram[0]) || schema.object_type(gram[0]).is_some() || gram[0].len() < 2) {
                continue;
            }
            if gram.iter().all(|w| is_stopword(w)) {
                continue;
            }
            let phrase = normalize(&gram.join(" "));
            let hits = g.by_name(&phrase);
            let confidence = if n > 1 || phrase.starts_with('#') { 1.0 } else { 0.9 };
            if push(hits, phrase, start, confidence, &mut found) {
                covered[start..start + n].iter_mut().for_each(|c| *c = true);
            }
        }
    }

    // 3. Fuzzy matching (typos, partial names) only when nothing matched exactly.
    if found.is_empty() && g.len() <= FUZZY_MAX_NAMES {
        let names: Vec<(&String, &[Id])> = g.names().filter(|(k, _)| k.len() >= 4 && !k.contains(':')).collect();
        for n in (1..=3usize.min(words.len())).rev() {
            for start in 0..=words.len() - n {
                if covered[start..start + n].iter().any(|c| *c) {
                    continue;
                }
                let gram: Vec<&str> = words[start..start + n].iter().map(String::as_str).collect();
                if gram.iter().any(|w| is_stopword(w) || schema.object_type(w).is_some()) {
                    continue;
                }
                let phrase = gram.join(" ");
                if phrase.len() < 4 {
                    continue;
                }
                let mut best: Option<(f32, &[Id])> = None;
                for (name, ids) in &names {
                    let s = similarity(&phrase, name);
                    if s >= FUZZY_THRESHOLD && best.is_none_or(|(b, _)| s > b) {
                        best = Some((s, ids));
                    }
                }
                if let Some((score, ids)) = best {
                    let cands: Vec<&Entity> = ids.iter().filter_map(|id| g.get(id)).collect();
                    if push(cands, phrase, start, (score * 0.85).min(0.85), &mut found) {
                        covered[start..start + n].iter_mut().for_each(|c| *c = true);
                    }
                }
            }
        }
    }

    // Dedupe (keep highest confidence), keep question order.
    let mut best: HashMap<Id, Resolved> = HashMap::new();
    for r in found {
        match best.get(&r.id) {
            Some(prev) if prev.confidence >= r.confidence => {}
            _ => {
                best.insert(r.id.clone(), r);
            }
        }
    }
    let mut out: Vec<Resolved> = best.into_values().collect();
    out.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| b.confidence.total_cmp(&a.confidence)));

    // "my"/"me"/"I" refers to the asker's own entity.
    let mentions_self = words.iter().any(|w| matches!(w.as_str(), "my" | "me" | "i" | "mine" | "am"));
    if mentions_self
        && let Some(me) = p.entity.as_deref().and_then(|id| g.get(id))
        && !out.iter().any(|r| r.id == me.id)
        && usable(me)
    {
        out.push(Resolved {
            id: me.id.clone(),
            kind: me.kind.clone(),
            title: me.label().to_string(),
            mention: "me".into(),
            confidence: 0.9,
            position: usize::MAX,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use onto_core::{ActionContext, EntityInput, Mutation};

    fn onto() -> Ontology {
        let o = Ontology::in_memory(Schema::enterprise());
        let ctx = ActionContext::system("acme");
        o.apply(
            &ctx,
            vec![
                Mutation::Upsert(EntityInput::new("project", "alpha", "Project Alpha")),
                Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
                Mutation::Upsert(EntityInput::new("issue", "421", "Payment timeout")),
                Mutation::Upsert(EntityInput::new("task", "421", "Investigate timeout")),
                Mutation::Upsert(EntityInput::new("person", "kenji", "Kenji Tanaka")),
                Mutation::Upsert(EntityInput::new("service", "postgres", "PostgreSQL")),
            ],
        )
        .unwrap();
        o
    }

    fn ids(o: &Ontology, q: &str) -> Vec<(String, f32)> {
        resolve(o, &Principal::admin("t"), "acme", q).into_iter().map(|r| (r.id, r.confidence)).collect()
    }

    #[test]
    fn resolves_names_hashtags_and_kinds() {
        let o = onto();
        assert_eq!(ids(&o, "Who owns Project Alpha?")[0].0, "project:alpha");
        assert_eq!(ids(&o, "Who is assigned to issue #421?"), vec![("issue:421".to_string(), 1.0)]);
        let ambiguous = ids(&o, "what about #421");
        assert_eq!(ambiguous.len(), 2);
        assert!(ambiguous.iter().all(|(_, c)| *c < 0.8));
        let two = ids(&o, "How is Kenji related to PostgreSQL?");
        assert_eq!(two.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["person:kenji", "service:postgres"]);
        assert_eq!(ids(&o, "How many issues does the Payments Team have")[0].0, "team:payments");
    }

    #[test]
    fn fuzzy_matches_typos_and_respects_workspace() {
        let o = onto();
        assert_eq!(ids(&o, "status of postgress")[0].0, "service:postgres");
        assert!(resolve(&o, &Principal::admin("t"), "other", "Project Alpha").is_empty());
    }
}
