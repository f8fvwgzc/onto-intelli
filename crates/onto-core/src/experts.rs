//! Expert finding: who knows about a topic, ranked from how people are
//! connected to the things that match it (assigned, owns, generated, team
//! ownership, recent activity), with the reasons.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::{Entity, EventFilter, Id};
use crate::schema::{Category, Dir};
use crate::time::now_ms;
use crate::{Ontology, Principal, Result, policy};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Expert {
    pub id: Id,
    pub title: String,
    pub kind: String,
    pub score: f32,
    pub reasons: Vec<String>,
}

fn rel_weight(rel: &str) -> (f32, &'static str) {
    match rel {
        "assigned_to" => (3.0, "assigned to"),
        "owns" => (2.5, "owns"),
        "generated_by" => (2.0, "generated"),
        "reported_by" => (1.0, "reported"),
        "requested_by" => (1.0, "requested"),
        "manages" => (1.5, "manages"),
        _ => (0.5, "connected to"),
    }
}

impl Ontology {
    /// Rank people and agents by their connection to `topic` (and `seeds`).
    pub fn experts(
        &self,
        p: &Principal,
        workspace: Option<&str>,
        topic: &str,
        seeds: &[Id],
        limit: usize,
    ) -> Result<Vec<Expert>> {
        let mut topics: Vec<(Id, f32)> = seeds.iter().map(|s| (self.resolve_id(s), 1.0)).collect();
        for (rank, hit) in self.search(p, topic, &[], workspace, 12)?.into_iter().enumerate() {
            if !topics.iter().any(|(t, _)| *t == hit.entity.id) {
                topics.push((hit.entity.id, 1.0 / (1.0 + rank as f32 * 0.3)));
            }
        }
        let since = now_ms() - 90 * 86_400_000;
        let mut activity: HashMap<Id, Vec<String>> = HashMap::new();
        for (t, _) in topics.iter().take(8) {
            let evs = self.store().events(&EventFilter {
                target: Some(t.clone()),
                since: Some(since),
                limit: Some(30),
                ..Default::default()
            })?;
            activity
                .insert(t.clone(), evs.into_iter().filter(|e| p.in_workspace(&e.workspace)).map(|e| e.actor).collect());
        }
        let mut scores: HashMap<Id, (f32, Vec<String>)> = HashMap::new();
        self.read(|g, schema| {
            let actor = |e: &Entity| schema.category(&e.kind) == Category::Actor && policy::visible(p, e, g);
            let mut add = |id: &str, w: f32, why: String| {
                let slot = scores.entry(id.to_string()).or_default();
                slot.0 += w;
                if !slot.1.contains(&why) {
                    slot.1.push(why);
                }
            };
            for (t, w) in &topics {
                let Some(te) =
                    g.get(t).filter(|e| policy::visible(p, e, g) && workspace.is_none_or(|ws| e.workspace == ws))
                else {
                    continue;
                };
                if schema.category(&te.kind) == Category::Actor {
                    continue;
                }
                for step in g.steps(t) {
                    let Some(o) = g.get(step.other) else { continue };
                    if actor(o) {
                        let (weight, verb) = rel_weight(step.rel);
                        add(&o.id, w * weight, format!("{verb} {}", te.label()));
                    } else if step.rel == "owns"
                        && step.dir == Dir::In
                        && schema.category(&o.kind) == Category::Container
                    {
                        // Team owns the topic: its members and managers know it too.
                        for m in g.steps_via(&o.id, "member_of", Dir::In) {
                            if let Some(me) = g.get(m.other).filter(|e| actor(e)) {
                                add(&me.id, w * 1.2, format!("member of {}, which owns {}", o.label(), te.label()));
                            }
                        }
                        for m in g.steps_via(&o.id, "manages", Dir::In) {
                            if let Some(me) = g.get(m.other).filter(|e| actor(e)) {
                                add(&me.id, w * 1.5, format!("manages {}, which owns {}", o.label(), te.label()));
                            }
                        }
                    }
                }
                for a in activity.get(t).into_iter().flatten().take(10) {
                    if let Some(ae) = g.get(a).filter(|e| actor(e)) {
                        add(&ae.id, w * 0.5, format!("recent activity on {}", te.label()));
                    }
                }
            }
        });
        let mut out: Vec<Expert> = self.read(|g, _| {
            scores
                .into_iter()
                .filter_map(|(id, (score, mut reasons))| {
                    let e = g.get(&id)?;
                    reasons.truncate(3);
                    Some(Expert {
                        title: e.label().to_string(),
                        kind: e.kind.clone(),
                        id,
                        score: (score * 100.0).round() / 100.0,
                        reasons,
                    })
                })
                .collect::<Vec<_>>()
        });
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        out.truncate(limit);
        Ok(out)
    }
}
