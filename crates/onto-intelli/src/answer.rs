//! Deterministic answers: questions the ontology can answer directly, with
//! zero LLM tokens and millisecond latency ("Who owns Project Alpha?").

use std::collections::BTreeMap;

use onto_core::deps::DepDirection;
use onto_core::metrics::{Measure, MetricFilter, MetricQuery, MetricResult};
use onto_core::query::FindSpec;
use onto_core::schema::{Category, Dir, Schema};
use onto_core::time::{fmt_date, now_ms};
use onto_core::{ActionContext, Entity, EntityPatch, EventFilter, Mutation, Ontology, Principal};

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
    question: &str,
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
        Intent::Count | Intent::List if c.group_by.is_some() => grouped(onto, p, ws, c, &confident, me()),
        Intent::Metric => metric_answer(onto, p, ws, c, &confident, me()),
        Intent::Expert => {
            let seeds: Vec<String> = confident
                .iter()
                .filter(|r| schema.category(&r.kind) != Category::Actor)
                .map(|r| r.id.clone())
                .collect();
            let topic = confident.first().map(|r| r.title.clone()).unwrap_or_else(|| question.to_string());
            let ex = onto.experts(p, Some(ws), question, &seeds, 5).ok()?;
            if ex.is_empty() {
                return Some(Direct { answer: format!("I couldn't find anyone connected to {topic}."), facts: vec![] });
            }
            let lines: Vec<String> =
                ex.iter().take(3).map(|e| format!("{} ({}) — {}", e.title, e.id, e.reasons.join("; "))).collect();
            Some(Direct {
                answer: format!("People who know about {topic}:\n• {}", lines.join("\n• ")), facts: lines
            })
        }
        Intent::Changes => {
            let days = c.since_days.unwrap_or(7);
            let since = now_ms() - i64::from(days) * 86_400_000;
            let within = first();
            let kinds: Vec<String> = c.kind.iter().cloned().collect();
            let ch = onto.changes(p, since, Some(ws), &kinds, within.as_ref().map(|e| e.id.as_str()), 25).ok()?;
            let scope = within.as_ref().map(|e| format!(" in {}", label(e))).unwrap_or_default();
            if ch.is_empty() {
                return Some(Direct { answer: format!("No changes{scope} in the last {days} day(s)."), facts: vec![] });
            }
            let lines: Vec<String> = ch
                .iter()
                .map(|x| {
                    let what = if x.created && !x.changed.is_empty() {
                        format!("created, then changed {}", x.changed.join(", "))
                    } else if x.created {
                        "created".to_string()
                    } else if x.deleted {
                        "deleted".to_string()
                    } else if x.from_state != x.to_state && x.to_state.is_some() {
                        format!("{} → {}", x.from_state.as_deref().unwrap_or("∅"), x.to_state.as_deref().unwrap_or("∅"))
                    } else {
                        format!("updated {}", x.changed.join(", "))
                    };
                    format!("{} ({}): {what} — by {}", x.title, x.id, x.actors.join(", "))
                })
                .collect();
            Some(Direct {
                answer: format!("{} change(s){scope} in the last {days} day(s):\n• {}", ch.len(), lines.join("\n• ")),
                facts: lines,
            })
        }
        Intent::Quality => {
            let q = question.to_lowercase();
            let focus = if q.contains("stale") {
                Some("stale")
            } else if q.contains("duplicate") || q.contains("dupe") {
                Some("duplicate")
            } else if q.contains("orphan") {
                Some("orphan")
            } else if q.contains("missing link") || q.contains("missing relation") {
                Some("missing_link")
            } else {
                None
            };
            let r = onto.quality(p, Some(ws), c.kind.as_deref(), 500).ok()?;
            let picked: Vec<String> = r
                .violations
                .iter()
                .filter(|v| focus.is_none_or(|f| v.rule == f))
                .take(10)
                .map(|v| v.message.clone())
                .collect();
            let answer = match focus {
                Some(f) if picked.is_empty() => format!("No {} problems found.", f.replace('_', " ")),
                Some(f) => format!("{} {} problem(s):\n• {}", picked.len(), f.replace('_', " "), picked.join("\n• ")),
                None => {
                    let rules: Vec<String> =
                        r.by_rule.iter().map(|(k, n)| format!("{n} {}", k.replace('_', " "))).collect();
                    format!(
                        "Data quality score {:.1}/100 across {} entities{}.{}",
                        r.score,
                        r.entities,
                        if rules.is_empty() { String::new() } else { format!(" — {}", rules.join(", ")) },
                        if picked.is_empty() { String::new() } else { format!("\n• {}", picked.join("\n• ")) }
                    )
                }
            };
            Some(Direct { answer, facts: picked })
        }
        Intent::WhatIf => what_if(onto, p, ws, c, first()?),
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
            let versions: Vec<String> = onto
                .history(p, &x.id, 6)
                .unwrap_or_default()
                .into_iter()
                .map(|h| format!("{} rev {} by {}: {}", &h.at[..10], h.revision, h.actor, h.changed.join(", ")))
                .collect();
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
            let events: Vec<_> = events
                .into_iter()
                .filter(|e| {
                    !(e.action.starts_with("entity.")
                        || e.action.starts_with("link.")
                        || e.action.starts_with("intelli."))
                })
                .collect();
            if events.is_empty() && versions.is_empty() {
                return Some(Direct { answer: format!("No recorded activity for {}.", label(&x)), facts: vec![] });
            }
            let mut lines: Vec<String> = events
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
            lines.extend(versions);
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
            let derived: Vec<String> = onto
                .derived(p, &x.id)
                .unwrap_or_default()
                .into_iter()
                .filter(|(k, v)| k != "age_days" && !v.is_null())
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            if !derived.is_empty() {
                answer.push_str(&format!("\nComputed — {}", derived.join(" · ")));
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

fn state_filter(c: &Classified) -> (Option<bool>, Vec<String>) {
    match &c.state {
        Some(StateFilter::Open) => (Some(true), vec![]),
        Some(StateFilter::Closed) => (Some(false), vec![]),
        Some(StateFilter::Is(s)) => (None, vec![s.clone()]),
        None => (None, vec![]),
    }
}

/// "issues", "work items", "people".
pub fn kind_label(kind: &str, n: usize) -> String {
    let base = kind.replace('_', " ");
    if n == 1 {
        base
    } else if base == "person" {
        "people".into()
    } else if base.ends_with('y') && !base.ends_with("ey") {
        format!("{}ies", &base[..base.len() - 1])
    } else if base.ends_with('s') {
        format!("{base}es")
    } else {
        format!("{base}s")
    }
}

fn scope_of(
    onto: &Ontology,
    p: &Principal,
    confident: &[&Resolved],
    kind: &str,
    mine: Option<Entity>,
) -> Option<Entity> {
    confident.iter().filter_map(|r| onto.get(p, &r.id).ok()).find(|e| !onto.schema().kind_is(&e.kind, kind)).or(mine)
}

fn rows_text(r: &MetricResult, unit: &str) -> Vec<String> {
    r.rows
        .iter()
        .map(|row| {
            let g: Vec<&str> = row.group.values().map(String::as_str).collect();
            format!("{}: {}{unit}", g.join(" / "), fmt_num(row.value))
        })
        .collect()
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 { format!("{x:.0}") } else { format!("{x:.2}") }
}

fn grouped(
    onto: &Ontology,
    p: &Principal,
    ws: &str,
    c: &Classified,
    confident: &[&Resolved],
    me: Option<Entity>,
) -> Option<Direct> {
    let dim = c.group_by.clone()?;
    let kind = c.kind.clone().unwrap_or_else(|| "work_item".into());
    let scope = scope_of(onto, p, confident, &kind, if c.mine { me } else { None });
    let (open, state) = state_filter(c);
    let q = MetricQuery {
        kind: Some(kind.clone()),
        measure: Some(Measure::Count),
        filter: Some(MetricFilter { open, state, props: Default::default() }),
        group_by: vec![dim.clone()],
        within: scope.as_ref().map(|e| e.id.clone()),
        workspace: Some(ws.to_string()),
        limit: Some(20),
        ..Default::default()
    };
    let r = onto.metric(p, &q).ok()?;
    let qualifier = match open {
        Some(true) => "Open ",
        Some(false) => "Closed ",
        None => "",
    };
    let scope_txt = scope.as_ref().map(|e| format!(" in {}", label(e))).unwrap_or_default();
    let lines = rows_text(&r, "");
    let answer = if r.count == 0 {
        format!("No {}{}{scope_txt}.", qualifier.to_lowercase(), kind_label(&kind, 2))
    } else {
        format!(
            "{qualifier}{} by {dim}{scope_txt} ({} in total):\n• {}",
            kind_label(&kind, 2),
            r.count,
            lines.join("\n• ")
        )
    };
    Some(Direct { answer: answer.trim_start().to_string(), facts: lines })
}

fn metric_answer(
    onto: &Ontology,
    p: &Principal,
    ws: &str,
    c: &Classified,
    confident: &[&Resolved],
    me: Option<Entity>,
) -> Option<Direct> {
    let schema = onto.schema();
    let measure = c.measure?;
    let named = schema.metrics.values().filter(|m| m.measure == measure).find(|m| match (&c.kind, &m.kind) {
        (Some(k), Some(mk)) => schema.kind_is(k, mk) || k == mk,
        (None, _) => true,
        (Some(_), None) => measure == Measure::FailureRate || measure == Measure::EventCount,
    });
    let kind = c.kind.clone().or_else(|| named.and_then(|m| m.kind.clone())).or_else(|| match measure {
        Measure::LeadTime => Some("issue".into()),
        Measure::Throughput => Some("work_item".into()),
        _ => None,
    });
    let scope = scope_of(onto, p, confident, kind.as_deref().unwrap_or("work_item"), if c.mine { me } else { None });
    let q = MetricQuery {
        metric: named.map(|m| m.name.clone()),
        kind: kind.clone(),
        measure: Some(measure),
        group_by: c.group_by.iter().cloned().collect(),
        within: scope.as_ref().map(|e| e.id.clone()),
        workspace: Some(ws.to_string()),
        window_days: c.since_days.or_else(|| named.and_then(|m| m.window_days)).or(Some(90)),
        event_action: named
            .and_then(|m| m.event_action.clone())
            .or_else(|| (measure == Measure::FailureRate).then(|| kind.clone().unwrap_or_else(|| "deployment".into()))),
        limit: Some(10),
        ..Default::default()
    };
    let r = onto.metric(p, &q).ok()?;
    let window = r.window_days.map(|d| format!(" over the last {d} days")).unwrap_or_default();
    let what = kind.as_deref().map(|k| kind_label(k, 2)).unwrap_or_else(|| "events".into());
    let certified = if r.certified { " [certified metric]" } else { "" };
    let head = match measure {
        Measure::LeadTime if r.count == 0 => format!("No {what} reached a finished state{window}."),
        Measure::LeadTime => {
            let p50 = r.rows.first().and_then(|x| x.p50).map(|v| format!(", median {v:.2}")).unwrap_or_default();
            let p90 = r.rows.first().and_then(|x| x.p90).map(|v| format!(", p90 {v:.2}")).unwrap_or_default();
            format!("Average lead time for {what}{window}: {:.2} days{p50}{p90} ({} finished).", r.value, r.count)
        }
        Measure::Throughput => {
            format!("{} {} finished{window}.", r.count, kind_label(kind.as_deref().unwrap_or("item"), r.count))
        }
        Measure::FailureRate => format!(
            "Failure rate{window}: {:.0}% of {} {} event(s) failed or were denied.",
            r.value * 100.0,
            r.count,
            q.event_action.clone().unwrap_or_default()
        ),
        Measure::EventCount => format!("{} event(s){window}.", r.count),
        _ => format!("{}: {}", r.metric, fmt_num(r.value)),
    };
    let lines = if c.group_by.is_some() || matches!(measure, Measure::FailureRate | Measure::EventCount) {
        rows_text(&r, if measure == Measure::FailureRate { " (rate)" } else { "" })
    } else {
        Vec::new()
    };
    let answer = if lines.is_empty() {
        format!("{head}{certified}")
    } else {
        format!("{head}{certified}\n• {}", lines.join("\n• "))
    };
    Some(Direct { answer, facts: lines })
}

/// Map a word like "done" or "fixed" to a real state of the kind.
pub fn hypothetical_state(schema: &Schema, kind: &str, word: &str) -> Option<String> {
    let ty = schema.object_type(kind)?;
    let w = word.to_lowercase().replace(' ', "_");
    if ty.states.contains(&w) {
        return Some(w);
    }
    let finished = [
        "done",
        "finished",
        "completed",
        "complete",
        "closed",
        "resolved",
        "fixed",
        "shipped",
        "delivered",
        "succeeded",
    ];
    if finished.contains(&w.as_str()) {
        if ty.states.iter().any(|s| s == "done") {
            return Some("done".into());
        }
        return ty.terminal_states.first().cloned();
    }
    let pick = |cands: &[&str]| cands.iter().find(|c| ty.states.iter().any(|s| s == *c)).map(|s| s.to_string());
    match w.as_str() {
        "blocked" | "stuck" => pick(&["blocked"]),
        "started" | "in_progress" | "progressing" | "underway" => pick(&["in_progress", "active", "running"]),
        "failed" | "broken" | "down" => pick(&["failed", "down", "degraded"]),
        "healthy" | "fixed_up" | "up" => pick(&["healthy", "succeeded"]),
        _ => None,
    }
}

fn what_if(onto: &Ontology, p: &Principal, ws: &str, c: &Classified, x: Entity) -> Option<Direct> {
    let schema = onto.schema();
    let state = hypothetical_state(schema, &x.kind, c.whatif_state.as_deref().unwrap_or("done"))?;
    let fork = onto.fork();
    fork.apply(
        &ActionContext::system(ws).with_source("scenario"),
        vec![Mutation::Patch(EntityPatch { id: x.id.clone(), state: Some(state.clone()), ..Default::default() })],
    )
    .ok()?;
    let mut lines = Vec::new();
    let containers: Vec<String> = onto.read(|g, _| {
        g.steps_via(&x.id, "belongs_to", Dir::Out)
            .into_iter()
            .chain(g.steps_via(&x.id, "part_of", Dir::Out))
            .map(|s| s.other.to_string())
            .collect()
    });
    for cid in containers {
        let (Ok(before), Ok(after)) = (onto.critical_path(p, &cid), fork.critical_path(p, &cid)) else { continue };
        if (before.cost - after.cost).abs() > f64::EPSILON {
            let path: Vec<&str> = after.path.iter().map(|n| n.id.as_str()).collect();
            let name = onto.get(p, &cid).map(|e| label(&e)).unwrap_or(cid.clone());
            lines.push(format!(
                "{name}: critical path {:.1} → {:.1} days{}",
                before.cost,
                after.cost,
                if path.is_empty() { String::new() } else { format!(" (then {})", path.join(" → ")) }
            ));
        }
    }
    if let Ok(down) = onto.dependencies(p, &x.id, DepDirection::Downstream, 4) {
        for n in down.nodes.iter().filter(|n| n.open) {
            let before = onto.dependencies(p, &n.id, DepDirection::Upstream, 4).map(|d| d.open.len()).unwrap_or(0);
            let after = fork.dependencies(p, &n.id, DepDirection::Upstream, 4).map(|d| d.open.len()).unwrap_or(0);
            if before > 0 && after == 0 {
                lines.push(format!("{} [{}] would have no open blockers", n.title, n.id));
            } else if after < before {
                lines.push(format!("{} [{}] would wait on {after} instead of {before} item(s)", n.title, n.id));
            }
        }
    }
    let answer = if lines.is_empty() {
        format!("If {} were {state}, nothing that depends on it would change.", label(&x))
    } else {
        format!("If {} were {state}:\n• {}", label(&x), lines.join("\n• "))
    };
    Some(Direct { answer, facts: lines })
}
