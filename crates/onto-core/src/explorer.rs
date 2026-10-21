//! Object Explorer: browse the objects of one type the way Palantir's Object
//! Explorer does. Filter by properties, state and links, search, sort and
//! page, and read facet counts and numeric statistics over the filtered set.
//!
//! Every read is permission-filtered: objects the caller may not see are
//! neither listed nor counted. Facets and statistics cover the whole
//! filtered set, not just the page.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::SearchMode;
use crate::graph::Graph;
use crate::model::Entity;
use crate::modeling::title_case;
use crate::policy::{self, Principal};
use crate::schema::{Backing, Category, PropType, Schema};
use crate::time::parse_ts;
use crate::{Error, Ontology, Result};

/// Page size when the spec gives none.
pub const DEFAULT_LIMIT: usize = 50;
/// Largest page.
pub const MAX_LIMIT: usize = 1000;
/// Search narrows the set to at most this many best matches.
pub const MAX_SEARCH_HITS: usize = 1000;
/// Values listed per facet.
pub const FACET_VALUES: usize = 12;
/// A string property is an automatic facet when it has at most this many
/// distinct values in the filtered set.
pub const MAX_AUTO_FACET_VALUES: usize = 50;
/// Histogram buckets per numeric property.
pub const BUCKETS: usize = 10;

/// Properties with a meaning of their own (access control), never columns.
const HIDDEN_PROPS: &[&str] = &["visibility", "acl"];

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

/// What to explore.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExploreSpec {
    /// Object type or interface.
    pub kind: String,
    #[serde(default)]
    pub workspace: Option<String>,
    /// Full-text search within the kind (the engine's lexical search).
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// `prop` ascending or `-prop` descending: a property or `title`,
    /// `state`, `id`, `created_at`, `updated_at` (a property of the same
    /// name wins; `_created_at` and so on always name the metadata).
    /// Default: relevance when searching, else title.
    #[serde(default)]
    pub sort: Option<String>,
    /// Page size: [`DEFAULT_LIMIT`] when 0, at most [`MAX_LIMIT`].
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    /// Facets to count; `None` picks them: the state, enum and bool
    /// properties, and string properties with few distinct values.
    #[serde(default)]
    pub facets: Option<Vec<String>>,
    /// Properties to summarize; `None` picks the int, float and timestamp
    /// properties.
    #[serde(default)]
    pub stats: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    In,
    Gt,
    Gte,
    Lt,
    Lte,
    Contains,
    Exists,
    Missing,
}

/// One condition; all filters of a spec must hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Filter {
    /// A property, `state`, `title`, `workspace`, `id`, `created_at`,
    /// `updated_at`, or `link:<rel>` (the ids linked through `rel`, which
    /// may be an inverse name). A property the kind declares wins over the
    /// metadata field of the same name; `_state`, `_created_at` and so on
    /// always name the metadata.
    pub prop: String,
    pub op: FilterOp,
    /// Compared value; a list for `in`. Timestamps accept dates
    /// (`2026-10-17`) as well as milliseconds.
    #[serde(default)]
    pub value: Value,
}

impl Filter {
    pub fn new(prop: impl Into<String>, op: FilterOp, value: Value) -> Self {
        Self { prop: prop.into(), op, value }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub label: String,
    pub ty: PropType,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FacetValue {
    pub value: Value,
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub lo: f64,
    pub hi: f64,
    pub count: usize,
}

/// Summary of a numeric or timestamp property over the filtered set.
/// Timestamps get count, min and max only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub count: usize,
    pub min: f64,
    pub max: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub histogram: Vec<Bucket>,
}

/// A link type the kind takes part in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkColumn {
    /// The relation as seen from the kind (an inverse name for incoming links).
    pub rel: String,
    pub label: String,
    /// Kinds at the other end.
    pub to: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExploreResult {
    pub kind: String,
    pub label: String,
    /// Objects matching the filters (before paging).
    pub total: usize,
    /// The page.
    pub objects: Vec<Entity>,
    pub columns: Vec<Column>,
    /// Field → its most frequent values (at most [`FACET_VALUES`]).
    pub facets: BTreeMap<String, Vec<FacetValue>>,
    pub stats: BTreeMap<String, Stats>,
    pub links: Vec<LinkColumn>,
    /// Action types with a parameter that takes this kind.
    pub actions: Vec<String>,
}

/// An object type and how many objects of it the caller can see.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectTypeSummary {
    pub name: String,
    pub label: String,
    pub category: Category,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing: Option<Backing>,
    /// Properties declared for it (with inherited ones).
    pub properties: usize,
    /// Link types it may be the source of.
    pub links: Vec<String>,
    pub actions: Vec<String>,
    #[serde(default)]
    pub description: String,
}

// ── fields and comparisons ───────────────────────────────────────────────

/// Entity fields that are not properties.
const META: &[&str] = &["title", "state", "workspace", "id", "created_at", "updated_at"];

/// A field of an object: entity metadata or a property. A property the kind
/// declares wins over metadata of the same name (a `created_at` column of
/// the source table is not the time the row was synced); the metadata is
/// always reachable with a leading underscore (`_created_at`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field<'a> {
    Meta(&'a str),
    Prop(&'a str),
}

impl<'a> Field<'a> {
    fn of(schema: &Schema, kind: &str, name: &'a str) -> Self {
        if let Some(m) = name.strip_prefix('_').filter(|m| META.contains(m)) {
            return Field::Meta(m);
        }
        // A backed type's state and title columns are the object's state and
        // title, not properties: `status = shipped` filters the state.
        if schema.prop_def(kind, name).is_none()
            && let Some(b) = schema.object_type(kind).and_then(|t| t.backing.as_ref())
        {
            if b.state.as_deref() == Some(name) {
                return Field::Meta("state");
            }
            if b.title.as_deref() == Some(name) {
                return Field::Meta("title");
            }
        }
        if META.contains(&name) && schema.prop_def(kind, name).is_none() {
            Field::Meta(name)
        } else {
            Field::Prop(name)
        }
    }

    fn get<'e>(self, e: &'e Entity) -> Option<Cow<'e, Value>> {
        match self {
            Field::Meta("title") => Some(Cow::Owned(Value::String(e.title.clone()))),
            Field::Meta("state") => e.state.as_ref().map(|s| Cow::Owned(Value::String(s.clone()))),
            Field::Meta("workspace") => Some(Cow::Owned(Value::String(e.workspace.clone()))),
            Field::Meta("id") => Some(Cow::Owned(Value::String(e.id.clone()))),
            Field::Meta("created_at") => Some(Cow::Owned(json!(e.created_at))),
            Field::Meta("updated_at") => Some(Cow::Owned(json!(e.updated_at))),
            Field::Meta(_) => None,
            Field::Prop(prop) => e.props.get(prop).filter(|v| !v.is_null()).map(Cow::Borrowed),
        }
    }

    /// The declared type of the field for `kind`.
    fn ty(self, schema: &Schema, kind: &str) -> Option<PropType> {
        match self {
            Field::Meta("created_at" | "updated_at") => Some(PropType::Timestamp),
            Field::Meta("state") => Some(PropType::Enum),
            Field::Meta(_) => Some(PropType::String),
            Field::Prop(prop) => schema.prop_def(kind, prop).map(|d| d.ty),
        }
    }
}

/// The name a result uses for metadata field `meta` of `kind`: the bare
/// name, or `_name` when the kind declares a property of that name.
fn meta_key(schema: &Schema, kind: &str, meta: &str) -> String {
    if schema.prop_def(kind, meta).is_some() { format!("_{meta}") } else { meta.to_string() }
}

fn number(v: &Value, timestamp: bool) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if timestamp => parse_ts(s).map(|x| x as f64),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Order of two values: numerically when either is a number (or the field
/// is a timestamp), else as text.
fn compare(a: &Value, b: &Value, timestamp: bool) -> Option<Ordering> {
    if (timestamp || a.is_number() || b.is_number())
        && let (Some(x), Some(y)) = (number(a, timestamp), number(b, timestamp))
    {
        return x.partial_cmp(&y);
    }
    match (a, b) {
        (Value::String(x), Value::String(y)) => Some(natural(x, y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Text order with digit runs compared as numbers: "order 9" < "order 10",
/// and keys such as 2 < 10 < 100 when objects have no title column.
fn natural(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let run = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits.trim_start_matches('0').to_string()
                };
                let (m, n) = (run(&mut x), run(&mut y));
                let o = m.len().cmp(&n.len()).then_with(|| m.cmp(&n));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                if c != d {
                    return c.cmp(&d);
                }
                x.next();
                y.next();
            }
        }
    }
}

fn equal(a: &Value, b: &Value, timestamp: bool) -> bool {
    match a {
        Value::Array(items) if !b.is_array() => items.iter().any(|x| equal(x, b, timestamp)),
        _ => a == b || compare(a, b, timestamp) == Some(Ordering::Equal),
    }
}

fn contains(a: &Value, needle: &str) -> bool {
    match a {
        Value::Array(items) => items.iter().any(|x| contains(x, needle)),
        other => text(other).to_lowercase().contains(needle),
    }
}

fn one_of(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

/// A filter, prepared for one kind.
struct Prepared<'a> {
    filter: &'a Filter,
    field: Field<'a>,
    /// `link:<rel>`: canonical relation and direction.
    link: Option<(String, crate::schema::Dir)>,
    timestamp: bool,
    needle: String,
}

impl<'a> Prepared<'a> {
    fn new(schema: &Schema, kind: &str, filter: &'a Filter) -> Result<Self> {
        let link = match filter.prop.strip_prefix("link:") {
            Some("") => return Err(Error::invalid("link filters name a relation: link:<rel>")),
            Some(rel) => Some(schema.resolve_rel_lenient(rel)),
            None => None,
        };
        if filter.op == FilterOp::In && !filter.value.is_array() {
            return Err(Error::invalid(format!("filter {} in: the value must be a list", filter.prop)));
        }
        if matches!(filter.op, FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte) && filter.value.is_null() {
            return Err(Error::invalid(format!("filter {} {:?}: needs a value", filter.prop, filter.op)));
        }
        let field = Field::of(schema, kind, &filter.prop);
        Ok(Self {
            filter,
            field,
            link,
            timestamp: field.ty(schema, kind) == Some(PropType::Timestamp),
            needle: text(&filter.value).to_lowercase(),
        })
    }

    /// Does `e` pass the filter, for `p`? Link filters see only the linked
    /// objects `p` may see: a hidden object neither matches nor counts.
    fn holds(&self, e: &Entity, g: &Graph, p: &Principal) -> bool {
        let f = self.filter;
        if let Some((base, dir)) = &self.link {
            let ids: Vec<Value> = g
                .neighbors(&e.id, base, *dir)
                .into_iter()
                .filter(|id| g.get(id).is_some_and(|n| policy::visible(p, n, g)))
                .map(|id| json!(id))
                .collect();
            let any = |pred: &dyn Fn(&Value) -> bool| ids.iter().any(pred);
            return match f.op {
                FilterOp::Exists => !ids.is_empty(),
                FilterOp::Missing => ids.is_empty(),
                FilterOp::Eq => any(&|id| id == &f.value),
                FilterOp::Ne => !any(&|id| id == &f.value),
                FilterOp::In => one_of(&f.value).iter().any(|v| ids.contains(v)),
                FilterOp::Contains => ids.iter().any(|id| {
                    let id = id.as_str().unwrap_or_default();
                    id.to_lowercase().contains(&self.needle)
                        || g.get(id).is_some_and(|o| o.title.to_lowercase().contains(&self.needle))
                }),
                FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte => false,
            };
        }
        let v = self.field.get(e);
        match (f.op, v) {
            (FilterOp::Exists, v) => v.is_some(),
            (FilterOp::Missing, v) => v.is_none(),
            (FilterOp::Ne, None) => true,
            (_, None) => false,
            (FilterOp::Eq, Some(v)) => equal(&v, &f.value, self.timestamp),
            (FilterOp::Ne, Some(v)) => !equal(&v, &f.value, self.timestamp),
            (FilterOp::In, Some(v)) => one_of(&f.value).iter().any(|x| equal(&v, x, self.timestamp)),
            (FilterOp::Contains, Some(v)) => contains(&v, &self.needle),
            (op, Some(v)) => {
                let ord = compare(&v, &f.value, self.timestamp);
                match op {
                    FilterOp::Gt => ord == Some(Ordering::Greater),
                    FilterOp::Gte => matches!(ord, Some(Ordering::Greater | Ordering::Equal)),
                    FilterOp::Lt => ord == Some(Ordering::Less),
                    _ => matches!(ord, Some(Ordering::Less | Ordering::Equal)),
                }
            }
        }
    }
}

// ── facets and statistics ────────────────────────────────────────────────

/// The most frequent values of a field, when `keep(distinct, counted)`
/// accepts the number of distinct values and of values counted.
fn facet(objects: &[&Entity], field: Field, keep: impl Fn(usize, usize) -> bool) -> Option<Vec<FacetValue>> {
    let mut counted = 0;
    let mut counts: HashMap<String, (Value, usize)> = HashMap::new();
    for e in objects {
        let Some(v) = field.get(e) else { continue };
        let values: Vec<&Value> = match v.as_ref() {
            Value::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        for x in values.into_iter().filter(|x| !x.is_null()) {
            counts.entry(x.to_string()).or_insert_with(|| (x.clone(), 0)).1 += 1;
            counted += 1;
        }
    }
    if counts.is_empty() || !keep(counts.len(), counted) {
        return None;
    }
    let mut list: Vec<FacetValue> = counts.into_values().map(|(value, count)| FacetValue { value, count }).collect();
    list.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| text(&a.value).cmp(&text(&b.value))));
    list.truncate(FACET_VALUES);
    Some(list)
}

fn stats(objects: &[&Entity], field: Field, timestamp: bool) -> Option<Stats> {
    let xs: Vec<f64> = objects
        .iter()
        .filter_map(|e| field.get(e).and_then(|v| number(&v, timestamp)))
        .filter(|x| x.is_finite())
        .collect();
    if xs.is_empty() {
        return None;
    }
    let min = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let max = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if timestamp {
        return Some(Stats { count: xs.len(), min, max, mean: None, sum: None, histogram: Vec::new() });
    }
    let sum: f64 = xs.iter().sum();
    let histogram = if max > min {
        let width = (max - min) / BUCKETS as f64;
        let mut b: Vec<Bucket> = (0..BUCKETS)
            .map(|i| Bucket {
                lo: min + width * i as f64,
                hi: if i + 1 == BUCKETS { max } else { min + width * (i + 1) as f64 },
                count: 0,
            })
            .collect();
        for x in &xs {
            let i = (((x - min) / width).floor() as usize).min(BUCKETS - 1);
            b[i].count += 1;
        }
        b
    } else {
        vec![Bucket { lo: min, hi: max, count: xs.len() }]
    };
    Some(Stats { count: xs.len(), min, max, mean: Some(sum / xs.len() as f64), sum: Some(sum), histogram })
}

// ── schema views ─────────────────────────────────────────────────────────

fn kind_label(schema: &Schema, kind: &str) -> String {
    match schema.object_types.get(kind) {
        Some(t) if !t.label.is_empty() => t.label.clone(),
        _ => title_case(kind),
    }
}

/// Link types `kind` may be the source of (outgoing), and the inverse names
/// of link types that may point at it (incoming).
fn link_columns(schema: &Schema, kind: &str) -> Vec<LinkColumn> {
    let admits = |list: &[String]| !list.is_empty() && list.iter().any(|k| schema.kind_is(kind, k));
    let mut out = Vec::new();
    for lt in schema.link_types.values() {
        if admits(&lt.from) {
            out.push(LinkColumn {
                rel: lt.name.clone(),
                label: if lt.label.is_empty() { title_case(&lt.name) } else { lt.label.clone() },
                to: lt.to.clone(),
            });
        }
        if admits(&lt.to)
            && let Some(inverse) = &lt.inverse
        {
            out.push(LinkColumn { rel: inverse.clone(), label: title_case(inverse), to: lt.from.clone() });
        }
    }
    out
}

/// Action types with an entity parameter that takes `kind`.
fn actions_for(schema: &Schema, kind: &str) -> Vec<String> {
    schema
        .action_types
        .values()
        .filter(|a| {
            a.parameters.values().any(|p| {
                p.ty == crate::action_types::ParamType::Entity && p.kinds.iter().any(|k| schema.kind_is(kind, k))
            })
        })
        .map(|a| a.name.clone())
        .collect()
}

fn columns(schema: &Schema, kind: &str) -> Vec<Column> {
    let mut out = vec![Column { name: meta_key(schema, kind, "title"), label: "Title".into(), ty: PropType::String }];
    if schema.object_types.get(kind).is_some_and(|t| !t.states.is_empty()) {
        out.push(Column { name: meta_key(schema, kind, "state"), label: "State".into(), ty: PropType::Enum });
    }
    for (name, def) in schema.all_props(kind) {
        if HIDDEN_PROPS.contains(&name.as_str()) {
            continue;
        }
        out.push(Column { name: name.clone(), label: title_case(name), ty: def.ty });
    }
    out
}

impl Ontology {
    /// Objects of one kind (or interface) that `p` may see, filtered,
    /// searched, sorted and paged, with facet counts and statistics over
    /// everything that matched.
    pub fn explore(&self, p: &Principal, spec: &ExploreSpec) -> Result<ExploreResult> {
        let kind = spec.kind.trim();
        if kind.is_empty() {
            return Err(Error::invalid("explore needs a kind (an object type)"));
        }
        let schema = self.schema();
        let ws = spec.workspace.as_deref().filter(|w| !w.is_empty());
        let search = spec.search.as_deref().map(str::trim).filter(|s| !s.is_empty());
        // Search runs first: it takes the graph lock itself.
        let ranked: Option<Vec<String>> = match search {
            Some(text) => Some(
                self.search_with(p, text, &[kind.to_string()], ws, MAX_SEARCH_HITS, SearchMode::Lexical, false)?
                    .into_iter()
                    .map(|h| h.entity.id)
                    .collect(),
            ),
            None => None,
        };
        let filters: Vec<Prepared> =
            spec.filters.iter().map(|f| Prepared::new(&schema, kind, f)).collect::<Result<_>>()?;
        let g = self.reader(p)?;
        let kinds: Vec<&String> = g.kinds().map(|(k, _)| k).filter(|k| schema.kind_is(k, kind)).collect();
        if kinds.is_empty() && !schema.object_types.contains_key(kind) && !schema.interfaces.contains_key(kind) {
            return Err(Error::not_found(format!("object type {kind}")));
        }
        let candidates: Vec<&Entity> = match &ranked {
            Some(ids) => ids.iter().filter_map(|id| g.get(id)).collect(),
            None => kinds.iter().flat_map(|k| g.of_kind(k)).collect(),
        };
        let mut matched: Vec<&Entity> = candidates
            .into_iter()
            .filter(|e| {
                schema.kind_is(&e.kind, kind)
                    && ws.is_none_or(|w| e.workspace == w)
                    && policy::visible(p, e, &g)
                    && filters.iter().all(|f| f.holds(e, &g, p))
            })
            .collect();

        // Sort: by the requested field (missing values last), else by
        // relevance when searching, else by title.
        let sort = spec.sort.as_deref().map(str::trim).filter(|s| !s.is_empty());
        match sort {
            Some(s) => {
                let (name, desc) = match s.strip_prefix('-') {
                    Some(n) => (n, true),
                    None => (s.strip_prefix('+').unwrap_or(s), false),
                };
                let field = Field::of(&schema, kind, name);
                let timestamp = field.ty(&schema, kind) == Some(PropType::Timestamp);
                let mut keyed: Vec<(Option<Value>, &Entity)> = matched
                    .into_iter()
                    .map(|e| {
                        let v = field.get(e).map(|v| match v.as_ref() {
                            Value::String(s) if !timestamp => Value::String(s.to_lowercase()),
                            other => other.clone(),
                        });
                        (v, e)
                    })
                    .collect();
                keyed.sort_by(|(a, ea), (b, eb)| {
                    let by_value = match (a, b) {
                        (Some(x), Some(y)) => {
                            let o = compare(x, y, timestamp).unwrap_or_else(|| text(x).cmp(&text(y)));
                            if desc { o.reverse() } else { o }
                        }
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => Ordering::Equal,
                    };
                    by_value.then_with(|| ea.id.cmp(&eb.id))
                });
                matched = keyed.into_iter().map(|(_, e)| e).collect();
            }
            None if ranked.is_some() => {}
            None => {
                let mut keyed: Vec<(String, &Entity)> =
                    matched.into_iter().map(|e| (e.label().to_lowercase(), e)).collect();
                keyed.sort_by(|(a, ea), (b, eb)| natural(a, b).then_with(|| ea.id.cmp(&eb.id)));
                matched = keyed.into_iter().map(|(_, e)| e).collect();
            }
        }

        // Facets and statistics over everything that matched.
        let ty = schema.object_types.get(kind);
        let unique: BTreeSet<&str> =
            ty.map(|t| t.constraints.unique.iter().map(String::as_str).collect()).unwrap_or_default();
        let mut facets = BTreeMap::new();
        match &spec.facets {
            Some(names) => {
                for name in names {
                    if let Some(list) = facet(&matched, Field::of(&schema, kind, name), |_, _| true) {
                        facets.insert(name.clone(), list);
                    }
                }
            }
            None => {
                if let Some(list) = facet(&matched, Field::Meta("state"), |_, _| true) {
                    facets.insert(meta_key(&schema, kind, "state"), list);
                }
                for (name, def) in schema.all_props(kind) {
                    if HIDDEN_PROPS.contains(&name.as_str()) || unique.contains(name.as_str()) {
                        continue;
                    }
                    let list = match def.ty {
                        PropType::Enum | PropType::Bool => facet(&matched, Field::Prop(name), |_, _| true),
                        // Few distinct values, and not identifier-like (a
                        // value of its own for every object).
                        PropType::String => facet(&matched, Field::Prop(name), |distinct, counted| {
                            distinct <= MAX_AUTO_FACET_VALUES && (counted <= 1 || distinct < counted)
                        }),
                        _ => None,
                    };
                    if let Some(list) = list {
                        facets.insert(name.clone(), list);
                    }
                }
            }
        }
        let mut summary = BTreeMap::new();
        let numeric: Vec<(String, Field)> = match &spec.stats {
            Some(names) => names.iter().map(|n| (n.clone(), Field::of(&schema, kind, n))).collect(),
            None => schema
                .all_props(kind)
                .into_iter()
                .filter(|(_, d)| matches!(d.ty, PropType::Int | PropType::Float | PropType::Timestamp))
                .map(|(n, _)| (n.clone(), Field::Prop(n)))
                .collect(),
        };
        for (name, field) in numeric {
            let timestamp = field.ty(&schema, kind) == Some(PropType::Timestamp);
            if let Some(s) = stats(&matched, field, timestamp) {
                summary.insert(name, s);
            }
        }

        let total = matched.len();
        let limit = if spec.limit == 0 { DEFAULT_LIMIT } else { spec.limit.min(MAX_LIMIT) };
        let objects = matched.into_iter().skip(spec.offset).take(limit).cloned().collect();
        Ok(ExploreResult {
            kind: kind.to_string(),
            label: kind_label(&schema, kind),
            total,
            objects,
            columns: columns(&schema, kind),
            facets,
            stats: summary,
            links: link_columns(&schema, kind),
            actions: actions_for(&schema, kind),
        })
    }

    /// Every object type with the number of its objects `p` can see (in
    /// `workspace`, or everywhere `p` may read).
    pub fn object_types(&self, p: &Principal, workspace: Option<&str>) -> Result<Vec<ObjectTypeSummary>> {
        let schema = self.schema();
        let g = self.reader(p)?;
        let ws = workspace.filter(|w| !w.is_empty());
        Ok(schema
            .object_types
            .values()
            .map(|t| ObjectTypeSummary {
                name: t.name.clone(),
                label: kind_label(&schema, &t.name),
                category: t.category,
                count: g
                    .of_kind(&t.name)
                    .filter(|e| ws.is_none_or(|w| e.workspace == w) && policy::visible(p, e, &g))
                    .count(),
                backing: t.backing.clone(),
                properties: schema.all_props(&t.name).len(),
                links: schema
                    .link_types
                    .values()
                    .filter(|lt| lt.from.iter().any(|k| schema.kind_is(&t.name, k)))
                    .map(|lt| lt.name.clone())
                    .collect(),
                actions: actions_for(&schema, &t.name),
                description: t.description.clone(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ActionContext;
    use crate::model::{EntityInput, LinkInput, Mutation};

    fn world() -> Ontology {
        let schema = Schema::from_toml(
            r#"
            [interfaces.priced]
            properties.amount = { type = "float" }

            [object_types.customer]
            label = "Customer"
            category = "actor"
            properties.email = { type = "string" }
            constraints.unique = ["email"]

            [object_types.order]
            label = "Order"
            implements = ["priced"]
            states = ["placed", "paid", "cancelled"]
            properties.amount = { type = "float" }
            properties.qty = { type = "int" }
            properties.placed_at = { type = "timestamp" }
            properties.channel = { type = "enum", values = ["web", "store"] }
            properties.gift = { type = "bool" }
            properties.region = { type = "string" }
            properties.code = { type = "string" }
            properties.notes = { type = "text" }
            backing = { source = "wh", table = "sales.orders", key = ["id"], sync = true, state = "status", title = "order_no" }

            [link_types.customer]
            from = ["order"]
            to = ["customer"]
            inverse = "orders"
            cardinality = "one"

            [action_types.cancel_order]
            parameters.item = { type = "entity", kinds = ["order"], required = true }
            effects = [{ patch = { id = "{item}", state = "cancelled" } }]
            "#,
        )
        .unwrap();
        let o = Ontology::in_memory(schema);
        let mut muts = vec![
            Mutation::Upsert(EntityInput::new("customer", "1", "Ada").prop("email", "ada@x.test")),
            Mutation::Upsert(EntityInput::new("customer", "2", "Bo").prop("email", "bo@x.test")),
        ];
        let regions = ["EU", "US", "APAC"];
        for i in 0..20_i64 {
            let state = ["placed", "paid", "cancelled"][(i % 3) as usize];
            let mut e = EntityInput::new("order", i.to_string(), format!("Order {i:02}"))
                .state(state)
                .prop("amount", 10.0 * i as f64)
                .prop("qty", i % 4)
                .prop("placed_at", 1_792_195_200_000_i64 + i * 86_400_000)
                .prop("channel", if i % 2 == 0 { "web" } else { "store" })
                .prop("gift", i % 5 == 0)
                .prop("region", regions[(i % 3) as usize])
                .prop("code", format!("C-{i}"));
            if i < 3 {
                e = e.prop("notes", "fragile, handle with care");
            }
            if i == 19 {
                e = e.prop("visibility", "restricted").prop("acl", json!(["team:vip"]));
            }
            e.workspace = Some("acme".into());
            muts.push(Mutation::Upsert(e));
        }
        for i in 0..10 {
            muts.push(Mutation::Link(LinkInput::new(format!("order:{i}"), "customer", "customer:1")));
        }
        muts.push(Mutation::Link(LinkInput::new("order:10", "customer", "customer:2")));
        let mut other = EntityInput::new("order", "beta-1", "Beta order").state("paid").prop("amount", 5.0);
        other.workspace = Some("beta".into());
        muts.push(Mutation::Upsert(other));
        o.apply(&ActionContext::new(Principal::admin("loader"), "acme"), muts).unwrap();
        o
    }

    fn admin() -> Principal {
        Principal::admin("ada")
    }

    fn member() -> Principal {
        Principal::named("user:bo", &["member"], &["acme"]).unwrap()
    }

    fn spec(kind: &str) -> ExploreSpec {
        ExploreSpec { kind: kind.into(), limit: DEFAULT_LIMIT, ..Default::default() }
    }

    fn with(filters: Vec<Filter>) -> ExploreSpec {
        ExploreSpec { filters, ..spec("order") }
    }

    fn ids(r: &ExploreResult) -> Vec<&str> {
        r.objects.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn lists_what_the_caller_may_see() {
        let o = world();
        let all = o.explore(&admin(), &spec("order")).unwrap();
        assert_eq!(all.total, 21, "both workspaces, restricted included");
        assert_eq!(all.label, "Order");
        assert_eq!(ids(&all)[0], "order:beta-1", "sorted by title");
        let acme = o.explore(&admin(), &ExploreSpec { workspace: Some("acme".into()), ..spec("order") }).unwrap();
        assert_eq!(acme.total, 20);
        let mine = o.explore(&member(), &spec("order")).unwrap();
        assert_eq!(mine.total, 19, "no restricted order, no other workspace");
        assert!(!ids(&mine).contains(&"order:19"));
        // Facets and statistics never count what the caller cannot see.
        assert_eq!(mine.stats["amount"].count, 19);
        assert_eq!(mine.stats["amount"].max, 180.0);
        let vip = o.explore(&member(), &with(vec![Filter::new("code", FilterOp::Eq, json!("C-19"))])).unwrap();
        assert_eq!(vip.total, 0);
        assert!(o.explore(&Principal::new("nobody", vec![], vec!["*".into()]), &spec("order")).is_err());

        // Interfaces list their implementers; unknown kinds are errors.
        assert_eq!(o.explore(&admin(), &spec("priced")).unwrap().total, 21);
        assert!(o.explore(&admin(), &spec("invoice")).unwrap_err().to_string().contains("invoice"));
        assert!(o.explore(&admin(), &spec(" ")).is_err());

        // Schema context: columns, links both ways, actions.
        let names: Vec<&str> = all.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(&names[..2], ["title", "state"]);
        assert!(names.contains(&"amount") && names.contains(&"placed_at"));
        assert_eq!(all.columns.iter().find(|c| c.name == "placed_at").unwrap().ty, PropType::Timestamp);
        assert_eq!(
            all.links,
            [LinkColumn { rel: "customer".into(), label: "Customer".into(), to: vec!["customer".into()] }]
        );
        assert_eq!(all.actions, ["cancel_order"]);
        let customers = o.explore(&admin(), &spec("customer")).unwrap();
        assert_eq!(customers.links[0].rel, "orders", "the inverse, seen from the customer");
        assert!(customers.actions.is_empty());
    }

    #[test]
    fn natural_order_compares_digit_runs_as_numbers() {
        let mut keys = vec!["10", "2", "100", "1", "order 10", "order 9", "a01", "a1"];
        keys.sort_by(|a, b| natural(a, b));
        assert_eq!(keys, ["1", "2", "10", "100", "a01", "a1", "order 9", "order 10"]);
    }

    #[test]
    fn filters_by_properties_states_and_links() {
        let o = world();
        let total = |filters: Vec<Filter>| o.explore(&admin(), &with(filters)).unwrap().total;
        assert_eq!(total(vec![Filter::new("state", FilterOp::Eq, json!("paid"))]), 7 + 1);
        assert_eq!(total(vec![Filter::new("state", FilterOp::Ne, json!("paid"))]), 13);
        assert_eq!(total(vec![Filter::new("state", FilterOp::In, json!(["placed", "cancelled"]))]), 13);
        // The backing's state column names the object state.
        assert_eq!(total(vec![Filter::new("status", FilterOp::Eq, json!("paid"))]), 7 + 1);
        assert_eq!(total(vec![Filter::new("amount", FilterOp::Gt, json!(150))]), 4);
        assert_eq!(total(vec![Filter::new("amount", FilterOp::Gte, json!("150"))]), 5, "numbers given as text");
        assert_eq!(total(vec![Filter::new("amount", FilterOp::Lt, json!(10))]), 2, "order:0 and beta-1");
        assert_eq!(total(vec![Filter::new("amount", FilterOp::Lte, json!(10))]), 3);
        // Timestamps compare with dates as well as milliseconds.
        assert_eq!(total(vec![Filter::new("placed_at", FilterOp::Lt, json!("2026-10-20"))]), 3);
        assert_eq!(
            total(vec![Filter::new("placed_at", FilterOp::Gte, json!(1_792_195_200_000_i64 + 18 * 86_400_000))]),
            2
        );
        assert_eq!(total(vec![Filter::new("created_at", FilterOp::Gt, json!("2000-01-01"))]), 21);
        assert_eq!(total(vec![Filter::new("region", FilterOp::Contains, json!("ap"))]), 6, "case-insensitive");
        assert_eq!(total(vec![Filter::new("title", FilterOp::Contains, json!("order 1"))]), 10);
        assert_eq!(total(vec![Filter::new("notes", FilterOp::Exists, Value::Null)]), 3);
        assert_eq!(total(vec![Filter::new("notes", FilterOp::Missing, Value::Null)]), 18);
        assert_eq!(total(vec![Filter::new("gift", FilterOp::Eq, json!(true))]), 4);
        assert_eq!(total(vec![Filter::new("workspace", FilterOp::Eq, json!("beta"))]), 1);
        // Several filters: all must hold.
        assert_eq!(
            total(vec![
                Filter::new("state", FilterOp::Eq, json!("paid")),
                Filter::new("channel", FilterOp::Eq, json!("web")),
            ]),
            3,
            "orders 4, 10 and 16"
        );
        // Links, also through the inverse name.
        assert_eq!(total(vec![Filter::new("link:customer", FilterOp::Eq, json!("customer:1"))]), 10);
        assert_eq!(total(vec![Filter::new("link:customer", FilterOp::In, json!(["customer:1", "customer:2"]))]), 11);
        assert_eq!(total(vec![Filter::new("link:customer", FilterOp::Missing, Value::Null)]), 10);
        assert_eq!(total(vec![Filter::new("link:customer", FilterOp::Ne, json!("customer:1"))]), 11);
        assert_eq!(total(vec![Filter::new("link:customer", FilterOp::Contains, json!("bo"))]), 1, "by the title");
        let busy = o
            .explore(
                &admin(),
                &ExploreSpec {
                    filters: vec![Filter::new("link:orders", FilterOp::Eq, json!("order:10"))],
                    ..spec("customer")
                },
            )
            .unwrap();
        assert_eq!(ids(&busy), ["customer:2"]);
        // Malformed filters are rejected, not ignored.
        assert!(o.explore(&admin(), &with(vec![Filter::new("state", FilterOp::In, json!("paid"))])).is_err());
        assert!(o.explore(&admin(), &with(vec![Filter::new("amount", FilterOp::Gt, Value::Null)])).is_err());
        assert!(o.explore(&admin(), &with(vec![Filter::new("link:", FilterOp::Exists, Value::Null)])).is_err());
        let parsed: ExploreSpec = serde_json::from_value(json!({
            "kind": "order", "filters": [{ "prop": "amount", "op": "gte", "value": 100 }, { "prop": "notes", "op": "missing" }]
        }))
        .unwrap();
        assert_eq!(parsed.limit, DEFAULT_LIMIT);
        assert_eq!(o.explore(&admin(), &parsed).unwrap().total, 10);
    }

    #[test]
    fn link_filters_never_see_hidden_objects() {
        let o = world();
        let ctx = ActionContext::new(admin(), "acme");
        o.apply(
            &ctx,
            vec![Mutation::Upsert(
                EntityInput::new("customer", "1", "Secret Person")
                    .prop("visibility", "restricted")
                    .prop("acl", json!(["team:hr"])),
            )],
        )
        .unwrap();
        let total = |p: &Principal, f: Filter| o.explore(p, &with(vec![f])).unwrap().total;
        assert!(o.get(&member(), "customer:1").is_err());
        let contains = Filter::new("link:customer", FilterOp::Contains, json!("secret p"));
        assert_eq!(total(&admin(), contains.clone()), 10);
        assert_eq!(total(&member(), contains), 0, "the hidden title is no oracle");
        assert_eq!(total(&member(), Filter::new("link:customer", FilterOp::Eq, json!("customer:1"))), 0);
        assert_eq!(total(&member(), Filter::new("link:customer", FilterOp::In, json!(["customer:1"]))), 0);
        // A hidden link counts as no link.
        assert_eq!(total(&member(), Filter::new("link:customer", FilterOp::Exists, Value::Null)), 1, "order:10");
        assert_eq!(total(&member(), Filter::new("link:customer", FilterOp::Missing, Value::Null)), 18);
        assert_eq!(total(&member(), Filter::new("link:customer", FilterOp::Ne, json!("customer:1"))), 19);
        // Seen from the hidden side's neighbours too.
        let r = o
            .explore(
                &member(),
                &ExploreSpec {
                    filters: vec![Filter::new("link:orders", FilterOp::Exists, Value::Null)],
                    ..spec("customer")
                },
            )
            .unwrap();
        assert_eq!(ids(&r), ["customer:2"]);
    }

    #[test]
    fn declared_properties_win_over_metadata_of_the_same_name() {
        let schema = Schema::from_toml(
            r#"
            [object_types.invoice]
            states = ["open", "paid"]
            properties.created_at = { type = "timestamp" }
            properties.state = { type = "string" }
            properties.title = { type = "string" }
            "#,
        )
        .unwrap();
        let o = Ontology::in_memory(schema);
        let mut muts = Vec::new();
        for (i, (created, state, st)) in [(1_000_i64, "CA", "open"), (5_000, "NY", "paid")].into_iter().enumerate() {
            muts.push(Mutation::Upsert(
                EntityInput::new("invoice", (i + 1).to_string(), format!("Invoice {}", i + 1))
                    .state(st)
                    .prop("created_at", created)
                    .prop("state", state)
                    .prop("title", "Net 30"),
            ));
        }
        o.apply(&ActionContext::new(Principal::admin("loader"), "acme"), muts).unwrap();
        let explore = |filters: Vec<Filter>, sort: Option<&str>| {
            o.explore(&admin(), &ExploreSpec { filters, sort: sort.map(str::to_string), ..spec("invoice") }).unwrap()
        };
        let total = |f: Filter| explore(vec![f], None).total;
        // The source's created_at, not the time the row was synced.
        assert_eq!(total(Filter::new("created_at", FilterOp::Lt, json!(2_000))), 1);
        assert_eq!(total(Filter::new("_created_at", FilterOp::Lt, json!(2_000))), 0, "metadata: synced now");
        assert_eq!(total(Filter::new("state", FilterOp::Eq, json!("CA"))), 1);
        assert_eq!(total(Filter::new("_state", FilterOp::Eq, json!("paid"))), 1);
        assert_eq!(total(Filter::new("title", FilterOp::Eq, json!("Net 30"))), 2);
        assert_eq!(total(Filter::new("_title", FilterOp::Contains, json!("invoice 2"))), 1);
        let r = explore(vec![], Some("-created_at"));
        assert_eq!(ids(&r), ["invoice:2", "invoice:1"]);
        assert_eq!(r.stats["created_at"].min, 1_000.0);
        assert!(r.facets.contains_key("_state") && !r.facets.contains_key("state"), "the automatic state facet");
        let picked = o
            .explore(&admin(), &ExploreSpec { facets: Some(vec!["state".into(), "_state".into()]), ..spec("invoice") })
            .unwrap();
        let values = |k: &str| picked.facets[k].iter().map(|f| f.value.clone()).collect::<Vec<_>>();
        assert_eq!(values("state"), [json!("CA"), json!("NY")], "the property");
        assert_eq!(values("_state"), [json!("open"), json!("paid")], "the object state");
        let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(&names[..2], ["_title", "_state"], "{names:?}");
        assert!(names.contains(&"title") && names.contains(&"state") && names.contains(&"created_at"));
    }

    #[test]
    fn facets_and_statistics_cover_the_filtered_set() {
        let o = world();
        let r = o.explore(&admin(), &ExploreSpec { workspace: Some("acme".into()), ..spec("order") }).unwrap();
        let keys: Vec<&str> = r.facets.keys().map(String::as_str).collect();
        assert_eq!(keys, ["channel", "gift", "region", "state"], "code is an identifier, notes is text");
        assert_eq!(
            r.facets["state"],
            [
                FacetValue { value: json!("paid"), count: 7 },
                FacetValue { value: json!("placed"), count: 7 },
                FacetValue { value: json!("cancelled"), count: 6 }
            ],
            "most frequent first, ties by value"
        );
        assert_eq!(
            r.facets["gift"],
            [FacetValue { value: json!(false), count: 16 }, FacetValue { value: json!(true), count: 4 }]
        );
        let a = &r.stats["amount"];
        assert_eq!((a.count, a.min, a.max), (20, 0.0, 190.0));
        assert_eq!(a.sum, Some(1900.0));
        assert_eq!(a.mean, Some(95.0));
        assert_eq!(a.histogram.len(), BUCKETS);
        assert_eq!(a.histogram.iter().map(|b| b.count).sum::<usize>(), 20);
        assert_eq!((a.histogram[0].lo, a.histogram[9].hi), (0.0, 190.0));
        assert_eq!(a.histogram[9].count, 2, "the maximum falls in the last bucket");
        assert_eq!(r.stats["qty"].max, 3.0);
        let t = &r.stats["placed_at"];
        assert_eq!(t.min, 1_792_195_200_000.0);
        assert!(t.mean.is_none() && t.sum.is_none() && t.histogram.is_empty(), "timestamps: min and max only");

        let paid = o.explore(&admin(), &with(vec![Filter::new("state", FilterOp::Eq, json!("paid"))])).unwrap();
        assert_eq!(paid.facets["state"], [FacetValue { value: json!("paid"), count: 8 }]);
        assert_eq!(paid.stats["amount"].count, 8);

        // Facets and statistics on request.
        let chosen = o
            .explore(
                &admin(),
                &ExploreSpec {
                    facets: Some(vec!["workspace".into(), "code".into()]),
                    stats: Some(vec!["qty".into(), "created_at".into()]),
                    ..spec("order")
                },
            )
            .unwrap();
        assert_eq!(chosen.facets.keys().collect::<Vec<_>>(), ["code", "workspace"]);
        assert_eq!(chosen.facets["code"].len(), FACET_VALUES, "top values only");
        assert_eq!(chosen.facets["workspace"][0], FacetValue { value: json!("acme"), count: 20 });
        assert_eq!(chosen.stats.keys().collect::<Vec<_>>(), ["created_at", "qty"]);
        assert!(chosen.stats["created_at"].mean.is_none());
        let single = o.explore(&admin(), &with(vec![Filter::new("code", FilterOp::Eq, json!("C-3"))])).unwrap();
        assert_eq!(single.stats["amount"].histogram, [Bucket { lo: 30.0, hi: 30.0, count: 1 }]);
    }

    #[test]
    fn sorts_pages_and_searches() {
        let o = world();
        let acme = |s: ExploreSpec| o.explore(&admin(), &ExploreSpec { workspace: Some("acme".into()), ..s }).unwrap();
        let r = acme(ExploreSpec { sort: Some("-amount".into()), limit: 3, ..spec("order") });
        assert_eq!(ids(&r), ["order:19", "order:18", "order:17"]);
        assert_eq!(r.total, 20, "the total ignores paging");
        let r = acme(ExploreSpec { sort: Some("amount".into()), limit: 2, offset: 5, ..spec("order") });
        assert_eq!(ids(&r), ["order:5", "order:6"]);
        let r = acme(ExploreSpec { sort: Some("notes".into()), ..spec("order") });
        assert!(r.objects[..3].iter().all(|e| e.props.contains_key("notes")), "missing values last");
        let r = acme(ExploreSpec { sort: Some("-notes".into()), ..spec("order") });
        assert!(r.objects[..3].iter().all(|e| e.props.contains_key("notes")), "also when descending");
        let r = acme(ExploreSpec { sort: Some("-placed_at".into()), limit: 1, ..spec("order") });
        assert_eq!(ids(&r), ["order:19"]);
        let r = acme(ExploreSpec { sort: Some("state".into()), limit: 1, ..spec("order") });
        assert_eq!(r.objects[0].state.as_deref(), Some("cancelled"));
        let r = acme(ExploreSpec { limit: 0, ..spec("order") });
        assert_eq!(r.objects.len(), 20.min(DEFAULT_LIMIT));
        assert_eq!(ids(&r)[..2], ["order:0", "order:1"], "by title by default");
        let r = acme(ExploreSpec { limit: 5000, ..spec("order") });
        assert_eq!(r.objects.len(), 20);

        // Search narrows to matches; filters still apply.
        let r = acme(ExploreSpec { search: Some("fragile".into()), ..spec("order") });
        assert_eq!(r.total, 3, "{:?}", ids(&r));
        let r = acme(ExploreSpec {
            search: Some("fragile".into()),
            filters: vec![Filter::new("amount", FilterOp::Gt, json!(0))],
            sort: Some("-amount".into()),
            ..spec("order")
        });
        assert_eq!(ids(&r), ["order:2", "order:1"]);
        assert_eq!(acme(ExploreSpec { search: Some("fragile".into()), ..spec("customer") }).total, 0);
    }

    #[test]
    fn summarizes_object_types() {
        let o = world();
        let types = o.object_types(&admin(), None).unwrap();
        let order = types.iter().find(|t| t.name == "order").unwrap();
        assert_eq!(order.count, 21);
        assert_eq!(order.label, "Order");
        assert_eq!(order.backing.as_ref().unwrap().table, "sales.orders");
        assert_eq!(order.properties, 8);
        assert_eq!(order.links, ["customer"]);
        assert_eq!(order.actions, ["cancel_order"]);
        let customer = types.iter().find(|t| t.name == "customer").unwrap();
        assert_eq!((customer.count, customer.category), (2, Category::Actor));
        assert!(customer.links.is_empty() && customer.backing.is_none());
        let mine = o.object_types(&member(), Some("acme")).unwrap();
        assert_eq!(mine.iter().find(|t| t.name == "order").unwrap().count, 19);
    }
}
