//! Deterministic answers: questions the ontology can answer directly, with
//! zero LLM tokens and millisecond latency ("Who owns Project Alpha?").

use std::collections::BTreeMap;

use onto_core::deps::DepDirection;
use onto_core::query::FindSpec;
use onto_core::schema::Category;
use onto_core::time::fmt_date;
use onto_core::{Entity, EventFilter, Ontology, Principal};

use crate::intent::{Classified, Intent, StateFilter};
use crate::memory::clip;
use crate::resolve::Resolved;

#[derive(Clone, Debug)]
pub struct Direct {
    pub answer: String,
    pub facts: Vec<String>,
}

fn label(e: &Entity) -> String {
    if e.title.is_empty() { e.id.clone() } else { format!("{} ({})", e.title, e.id) }
}

fn join(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        2 => format!("{} and {}", items[0], items[1]),
        n => format!("{}, and {}", items[..n - 1].join(", "), items[n - 1]),
    }
}

fn plural(kind: &str, n: usize) -> String {
    if n == 1 {
        kind.to_string()
    } else if kind.ends_with('s') || kind.ends_with('x') {
        format!("{kind}es")
    } else {
        format!("{kind}s")
    }
}

pub fn try_answer(
    onto: &Ontology,
    p: &Principal,
    ws: &str,
    c: &Classified,
    ents: &[Resolved],
    min_conf: f32,
) -> Option<Direct> {
    let confident: Vec<&Resolved> = ents.iter().filter(|r| r.confidence >= min_conf).collect();
    let first = || -> Option<Entity> { onto.get(p, &confident.first()?.id).ok() };
    let me = || -> Option<Entity> { p.entity.as_deref().and_then(|id| onto.get(p, id).ok()) };
    let schema = onto.schema();

    match c.intent {
        Intent::Owner => {
            let x = first()?;
            let owners = onto.neighbors(p, &x.id, Some("owned_by")).ok()?;
            if !owners.is_empty() {
                let names: Vec<String> = owners.iter().map(|(_, e)| label(e)).collect();
                let facts = owners.iter().map(|(r, e)| format!("{} {r} {}", x.id, e.id)).collect();
                return Some(Direct { answer: format!("{} is owned by {}.", label(&x), join(&names)), facts });
            }
            let inferred: Vec<_> = onto
                .infer(p, &x.id)
                .ok()?
                .into_iter()
                .filter(|i| matches!(i.rel.as_str(), "owning_team" | "accountable_team"))
                .collect();
            if let Some(inf) = inferred.first() {
                let team = onto.get(p, &inf.to).ok()?;
                let path = onto.path_text(&inf.because);
                return Some(Direct {
                    answer: format!(
                        "{} has no direct owner; it is owned by {} through {path}.",
                        label(&x),
                        label(&team)
                    ),
                    facts: vec![format!("derived: {} {} {}", inf.from, inf.rel, inf.to)],
                });
            }
            Some(Direct { answer: format!("{} has no recorded owner.", label(&x)), facts: vec![] })
        }
        Intent::Assignee => {
            let x = first()?;
            let assignees = onto.neighbors(p, &x.id, Some("assigned_to")).ok()?;
            let mut facts: Vec<String> = assignees.iter().map(|(r, e)| format!("{} {r} {}", x.id, e.id)).collect();
            let mut answer = if assignees.is_empty() {
                format!("{} is not assigned to anyone.", label(&x))
            } else {
                let names: Vec<String> = assignees.iter().map(|(_, e)| label(e)).collect();
                format!("{} is assigned to {}.", label(&x), join(&names))
            };
            for (_, a) in &assignees {
                let teams = onto.neighbors(p, &a.id, Some("member_of")).unwrap_or_default();
                if !teams.is_empty() {
                    let t: Vec<String> = teams.iter().map(|(_, e)| label(e)).collect();
                    answer.push_str(&format!(" {} is a member of {}.", a.label(), join(&t)));
                    facts.extend(teams.iter().map(|(_, e)| format!("{} member_of {}", a.id, e.id)));
                }
            }
            for inf in onto.infer(p, &x.id).unwrap_or_default() {
                let relevant = inf.rel == "accountable_team" || (assignees.is_empty() && inf.rel == "escalate_to");
                if relevant && let Ok(t) = onto.get(p, &inf.to) {
                    let verb = if inf.rel == "escalate_to" {
                        "Escalate to"
                    } else {
                        "Accountable team (owns the affected system):"
                    };
                    answer.push_str(&format!(" {verb} {}.", label(&t)));
                    facts.push(format!("derived: {} {} {}", inf.from, inf.rel, inf.to));
                }
            }
            Some(Direct { answer, facts })
        }
        Intent::Status => {
            let x = first()?;
            let mut answer = match &x.state {
                Some(s) => format!("{} is {s} (last updated {}).", label(&x), fmt_date(x.updated_at)),
                None => format!("{} has no tracked state (last updated {}).", label(&x), fmt_date(x.updated_at)),
            };
            let mut facts = vec![format!("{} state {}", x.id, x.state.clone().unwrap_or_default())];
            let deps = onto.dependencies(p, &x.id, DepDirection::Upstream, 3).ok()?;
            let open: Vec<String> = deps
                .nodes
                .iter()
                .filter(|n| n.open)
                .map(|n| {
                    format!("{} [{}]{}", n.title, n.id, n.state.as_ref().map(|s| format!(" ({s})")).unwrap_or_default())
                })
                .collect();
            if !open.is_empty() {
                answer.push_str(&format!(" It is waiting on {} unfinished item(s): {}.", open.len(), join(&open)));
                facts.extend(open.iter().map(|o| format!("{} waits on {o}", x.id)));
            }
            if schema.is_alert(&x.kind, x.state.as_deref())
                && let Some(chain) = onto.root_cause(p, &x.id, 5).ok()?.chains.first()
            {
                answer.push_str(&format!(" Likely cause: {}.", chain.text));
                facts.push(format!("cause chain: {}", chain.text));
            }
            Some(Direct { answer, facts })
        }
        Intent::Members => {
            let team = confident
                .iter()
                .filter_map(|r| onto.get(p, &r.id).ok())
                .find(|e| schema.category(&e.kind) == Category::Container)?;
            let members = onto.neighbors(p, &team.id, Some("has_member")).ok()?;
            let managers = onto.neighbors(p, &team.id, Some("managed_by")).unwrap_or_default();
            let names: Vec<String> = members.iter().map(|(_, e)| label(e)).collect();
            let mut answer = if names.is_empty() {
                format!("{} has no recorded members.", label(&team))
            } else {
                format!("{} has {} {}: {}.", label(&team), names.len(), plural("member", names.len()), join(&names))
            };
            if !managers.is_empty() {
                let m: Vec<String> = managers.iter().map(|(_, e)| label(e)).collect();
                answer.push_str(&format!(" Managed by {}.", join(&m)));
            }
            let facts = members.iter().map(|(_, e)| format!("{} member_of {}", e.id, team.id)).collect();
            Some(Direct { answer, facts })
        }
        Intent::Count | Intent::List => {
            let kind = c.kind.clone()?;
            let scope = confident
                .iter()
                .filter_map(|r| onto.get(p, &r.id).ok())
                .find(|e| e.kind != kind)
                .or_else(|| if c.mine { me() } else { None });
            let (open, state) = match &c.state {
                Some(StateFilter::Open) => (Some(true), vec![]),
                Some(StateFilter::Closed) => (Some(false), vec![]),
                Some(StateFilter::Is(s)) => (None, vec![s.clone()]),
                None => (None, vec![]),
            };
            let spec = FindSpec {
                kind: Some(kind.clone()),
                workspace: Some(ws.to_string()),
                state: state.clone(),
                open,
                within: scope.as_ref().map(|s| s.id.clone()),
                limit: 15,
                ..Default::default()
            };
            let (items, total) = onto.find(p, &spec).ok()?;
            let qualifier = match (&c.state, open) {
                (Some(StateFilter::Is(s)), _) => format!("{s} "),
                (_, Some(true)) => "open ".into(),
                (_, Some(false)) => "closed ".into(),
                _ => String::new(),
            };
            let scope_label = scope.as_ref().map(label).unwrap_or_else(|| format!("Workspace {ws}"));
            let lines: Vec<String> = items
                .iter()
                .map(|e| format!("{}{}", label(e), e.state.as_ref().map(|s| format!(" — {s}")).unwrap_or_default()))
                .collect();
            let facts = lines.clone();
            let answer = if c.intent == Intent::Count {
                let mut a = format!("{scope_label} has {total} {qualifier}{}.", plural(&kind, total));
                if total > 0 && total <= 10 {
                    a.push_str(&format!(" {}.", join(&items.iter().map(label).collect::<Vec<_>>())));
                }
                a
            } else if total == 0 {
                format!("No {qualifier}{} in {scope_label}.", plural(&kind, 2))
            } else {
                let mut a =
                    format!("{total} {qualifier}{} in {scope_label}:\n• {}", plural(&kind, total), lines.join("\n• "));
                if total > items.len() {
                    a.push_str(&format!("\n…and {} more", total - items.len()));
                }
                a
            };
            Some(Direct { answer, facts })
        }
        Intent::Blockers if !c.why => {
            let x = first()?;
            let deps = onto.dependencies(p, &x.id, DepDirection::Upstream, 4).ok()?;
            let open: Vec<String> = deps
                .nodes
                .iter()
                .filter(|n| n.open)
                .map(|n| {
                    format!("{} [{}]{}", n.title, n.id, n.state.as_ref().map(|s| format!(" ({s})")).unwrap_or_default())
                })
                .collect();
            let rc = onto.root_cause(p, &x.id, 5).ok()?;
            let mut facts: Vec<String> = open.iter().map(|o| format!("{} blocked by {o}", x.id)).collect();
            let mut answer = if open.is_empty() {
                format!("{} has no open blockers.", label(&x))
            } else {
                format!("{} is blocked by {}.", label(&x), join(&open))
            };
            if let Some(chain) = rc.chains.first() {
                answer.push_str(&format!("\nRoot-cause chain: {}.", chain.text));
                facts.push(format!("cause chain: {}", chain.text));
            }
            Some(Direct { answer, facts })
        }
        Intent::Dependencies => {
            let x = first()?;
            let deps = onto.dependencies(p, &x.id, DepDirection::Upstream, 4).ok()?;
            if deps.nodes.is_empty() {
                return Some(Direct { answer: format!("{} has no recorded dependencies.", label(&x)), facts: vec![] });
            }
            let items: Vec<String> = deps
                .nodes
                .iter()
                .map(|n| {
                    format!(
                        "{} [{}]{}{}",
                        n.title,
                        n.id,
                        n.state.as_ref().map(|s| format!(" ({s})")).unwrap_or_default(),
                        if n.depth > 1 { " — transitive" } else { "" }
                    )
                })
                .collect();
            let mut answer = format!("{} depends on {}.", label(&x), join(&items));
            if !deps.cycles.is_empty() {
                answer.push_str(&format!(" Warning: dependency cycle {}.", deps.cycles[0].join(" → ")));
            }
            Some(Direct { answer, facts: items })
        }
        Intent::Impact => {
            let x = first()?;
            let rep = onto.impact(p, &x.id, 4).ok()?;
            if rep.nodes.is_empty() {
                return Some(Direct {
                    answer: format!("Nothing in the graph depends on {}.", label(&x)),
                    facts: vec![],
                });
            }
            let items: Vec<String> = rep
                .nodes
                .iter()
                .map(|n| {
                    format!("{} [{}]{}", n.title, n.id, n.state.as_ref().map(|s| format!(" ({s})")).unwrap_or_default())
                })
                .collect();
            Some(Direct {
                answer: format!(
                    "If {} fails or slips, {} {} affected: {}.",
                    label(&x),
                    items.len(),
                    if items.len() == 1 { "item is" } else { "items are" },
                    join(&items)
                ),
                facts: items,
            })
        }
        Intent::Relation => {
            if confident.len() < 2 {
                return None;
            }
            let (a, b) = (onto.get(p, &confident[0].id).ok()?, onto.get(p, &confident[1].id).ok()?);
            match onto.path(p, &a.id, &b.id, 5).ok()? {
                Some(hops) => {
                    let text = onto.path_text(&hops);
                    Some(Direct {
                        answer: format!("{} is connected to {}: {text}.", label(&a), label(&b)),
                        facts: vec![text],
                    })
                }
                None => Some(Direct {
                    answer: format!("No connection between {} and {} within 5 hops.", label(&a), label(&b)),
                    facts: vec![],
                }),
            }
        }
        Intent::Workload => {
            let person = confident
                .iter()
                .filter_map(|r| onto.get(p, &r.id).ok())
                .find(|e| schema.category(&e.kind) == Category::Actor)
                .or_else(|| if c.mine { me() } else { None })?;
            let items: Vec<Entity> = onto
                .neighbors(p, &person.id, Some("works_on"))
                .ok()?
                .into_iter()
                .map(|(_, e)| e)
                .filter(|e| schema.is_open(&e.kind, e.state.as_deref()))
                .collect();
            if items.is_empty() {
                return Some(Direct {
                    answer: format!("{} has no open assigned work.", label(&person)),
                    facts: vec![],
                });
            }
            let lines: Vec<String> = items
                .iter()
                .map(|e| format!("{}{}", label(e), e.state.as_ref().map(|s| format!(" — {s}")).unwrap_or_default()))
                .collect();
            Some(Direct {
                answer: format!(
                    "{} is working on {} open {}:\n• {}",
                    label(&person),
                    items.len(),
                    plural("item", items.len()),
                    lines.join("\n• ")
                ),
                facts: lines,
            })
        }
        Intent::History => {
            let x = first()?;
            let events = onto
                .events(
                    p,
                    &EventFilter {
                        target: Some(x.id.clone()),
                        workspace: Some(ws.to_string()),
                        limit: Some(8),
                        ..Default::default()
                    },
                )
                .ok()?;
            if events.is_empty() {
                return Some(Direct { answer: format!("No recorded activity for {}.", label(&x)), facts: vec![] });
            }
            let lines: Vec<String> = events
                .iter()
                .map(|e| {
                    let detail = e
                        .payload
                        .as_object()
                        .and_then(|o| {
                            ["reason", "error", "changed"]
                                .iter()
                                .find_map(|k| o.get(*k).filter(|v| v.as_array().is_none_or(|a| !a.is_empty())))
                        })
                        .map(|v| format!(" — {}", clip(&v.to_string(), 80)))
                        .unwrap_or_default();
                    format!("{} {} {} ({}){detail}", fmt_date(e.ts), e.actor, e.action, e.outcome.as_str())
                })
                .collect();
            Some(Direct {
                answer: format!("Recent activity on {}:\n• {}", label(&x), lines.join("\n• ")),
                facts: lines,
            })
        }
        Intent::Lookup => {
            let x = first()?;
            let mut answer = format!("{} is a {}", label(&x), x.kind);
            if let Some(s) = &x.state {
                answer.push_str(&format!(", currently {s}"));
            }
            answer.push('.');
            let props: Vec<String> = x
                .props
                .iter()
                .filter(|(k, _)| !matches!(k.as_str(), "visibility" | "acl"))
                .take(6)
                .map(|(k, v)| {
                    format!(
                        "{k}: {}",
                        clip(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()).as_str(), 140)
                    )
                })
                .collect();
            if !props.is_empty() {
                answer.push_str(&format!("\n{}", props.join(" · ")));
            }
            let mut by_rel: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (rel, e) in onto.neighbors(p, &x.id, None).unwrap_or_default() {
                by_rel.entry(rel).or_default().push(e.label().to_string());
            }
            let rels: Vec<String> = by_rel
                .iter()
                .take(8)
                .map(|(rel, names)| {
                    let shown: Vec<String> = names.iter().take(4).cloned().collect();
                    let more = if names.len() > 4 { format!(" +{}", names.len() - 4) } else { String::new() };
                    format!("{rel}: {}{more}", shown.join(", "))
                })
                .collect();
            if !rels.is_empty() {
                answer.push_str(&format!("\nRelations — {}", rels.join("; ")));
            }
            if let Some(pv) = &x.provenance {
                answer.push_str(&format!(
                    "\nProvenance — source: {}",
                    if pv.source.is_empty() { "unknown" } else { &pv.source }
                ));
                if !pv.derived_from.is_empty() {
                    answer.push_str(&format!("; derived from {}", pv.derived_from.join(", ")));
                }
                if let Some(g) = &pv.generated_by {
                    answer.push_str(&format!("; generated by {g}"));
                }
            }
            Some(Direct { answer, facts: rels })
        }
        _ => None,
    }
}
