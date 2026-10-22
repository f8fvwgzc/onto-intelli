//! In-memory typed graph with adjacency and name indexes.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasher, RandomState};

use hashbrown::HashTable;

use crate::model::{Entity, Id, Link};
use crate::schema::Dir;

pub type LinkKey = (Id, String, Id);

/// Entities and links sit in key-less hash tables: an entity is found by
/// its own `id` and a link by its own `from`, `rel` and `to`, so no key is
/// stored twice.
#[derive(Clone, Default)]
pub struct Graph {
    /// Hashes entity ids and link keys for both tables (random per graph,
    /// shared by its clones).
    hasher: RandomState,
    entities: HashTable<Entity>,
    links: HashTable<Link>,
    out: HashMap<Id, Vec<(String, Id)>>,
    inc: HashMap<Id, Vec<(String, Id)>>,
    by_kind: HashMap<String, BTreeSet<Id>>,
    names: HashMap<String, NameIds>,
}

/// The ids one name key refers to. Almost every key names a single entity,
/// so that case is stored inline instead of in a `BTreeSet` leaf of its own.
/// `Many` is sorted ascending and holds no duplicates.
#[derive(Clone, Debug)]
enum NameIds {
    One(Id),
    Many(Vec<Id>),
}

impl NameIds {
    fn insert(&mut self, id: &str) {
        match self {
            NameIds::One(x) if x == id => {}
            NameIds::One(x) => {
                let x = std::mem::take(x);
                let id = id.to_string();
                *self = NameIds::Many(if x < id { vec![x, id] } else { vec![id, x] });
            }
            NameIds::Many(v) => {
                if let Err(i) = v.binary_search_by(|x| x.as_str().cmp(id)) {
                    v.insert(i, id.to_string());
                }
            }
        }
    }

    /// Removes `id`; true when no id is left.
    fn remove(&mut self, id: &str) -> bool {
        match self {
            NameIds::One(x) => x == id,
            NameIds::Many(v) => {
                if let Ok(i) = v.binary_search_by(|x| x.as_str().cmp(id)) {
                    v.remove(i);
                }
                match v.len() {
                    0 => true,
                    1 => {
                        let last = v.pop().unwrap_or_default();
                        *self = NameIds::One(last);
                        false
                    }
                    _ => false,
                }
            }
        }
    }

    fn as_slice(&self) -> &[Id] {
        match self {
            NameIds::One(x) => std::slice::from_ref(x),
            NameIds::Many(v) => v,
        }
    }
}

/// One step from a node: relation, neighbour, and whether the stored edge
/// points away from the node (`Dir::Out`) or towards it (`Dir::In`).
#[derive(Clone, Copy, Debug)]
pub struct Step<'a> {
    pub rel: &'a str,
    pub other: &'a str,
    pub dir: Dir,
}

fn entity_hash(hasher: &RandomState, id: &str) -> u64 {
    hasher.hash_one(id)
}

/// Stored links and probe keys both hash this `&str` triple, so they agree.
fn link_hash(hasher: &RandomState, from: &str, rel: &str, to: &str) -> u64 {
    hasher.hash_one((from, rel, to))
}

fn is_link(l: &Link, from: &str, rel: &str, to: &str) -> bool {
    l.from == from && l.rel == rel && l.to == to
}

impl Graph {
    pub fn get(&self, id: &str) -> Option<&Entity> {
        self.entities.find(entity_hash(&self.hasher, id), |e| e.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    pub fn entities(&self) -> impl Iterator<Item = &Entity> {
        self.entities.iter()
    }

    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.links.iter()
    }

    fn take_entity(&mut self, id: &str) -> Option<Entity> {
        let h = entity_hash(&self.hasher, id);
        let found = self.entities.find_entry(h, |e| e.id == id).ok()?;
        Some(found.remove().0)
    }

    pub fn put_entity(&mut self, e: Entity) {
        if let Some(old) = self.take_entity(&e.id) {
            self.unindex(&old);
        }
        self.index(&e);
        let hasher = &self.hasher;
        self.entities.insert_unique(entity_hash(hasher, &e.id), e, |x| entity_hash(hasher, &x.id));
    }

    /// Removes the entity and every link touching it.
    pub fn remove_entity(&mut self, id: &str) -> Option<Entity> {
        let e = self.take_entity(id)?;
        self.unindex(&e);
        for key in self.incident_keys(id) {
            self.remove_link(&key);
        }
        Some(e)
    }

    pub fn incident_keys(&self, id: &str) -> Vec<LinkKey> {
        let mut keys: Vec<LinkKey> =
            self.out_edges(id).iter().map(|(r, t)| (id.to_string(), r.clone(), t.clone())).collect();
        keys.extend(self.in_edges(id).iter().map(|(r, f)| (f.clone(), r.clone(), id.to_string())));
        keys
    }

    pub fn put_link(&mut self, link: Link) {
        let hasher = &self.hasher;
        let h = link_hash(hasher, &link.from, &link.rel, &link.to);
        match self.links.find_entry(h, |l| is_link(l, &link.from, &link.rel, &link.to)) {
            Ok(mut found) => *found.get_mut() = link,
            Err(_) => {
                self.out.entry(link.from.clone()).or_default().push((link.rel.clone(), link.to.clone()));
                self.inc.entry(link.to.clone()).or_default().push((link.rel.clone(), link.from.clone()));
                self.links.insert_unique(h, link, |l| link_hash(hasher, &l.from, &l.rel, &l.to));
            }
        }
    }

    pub fn remove_link(&mut self, key: &LinkKey) -> Option<Link> {
        let h = link_hash(&self.hasher, &key.0, &key.1, &key.2);
        let found = self.links.find_entry(h, |l| is_link(l, &key.0, &key.1, &key.2)).ok()?;
        let link = found.remove().0;
        if let Some(v) = self.out.get_mut(&key.0) {
            v.retain(|(r, t)| !(r == &key.1 && t == &key.2));
            if v.is_empty() {
                self.out.remove(&key.0);
            }
        }
        if let Some(v) = self.inc.get_mut(&key.2) {
            v.retain(|(r, f)| !(r == &key.1 && f == &key.0));
            if v.is_empty() {
                self.inc.remove(&key.2);
            }
        }
        Some(link)
    }

    pub fn get_link(&self, from: &str, rel: &str, to: &str) -> Option<&Link> {
        self.links.find(link_hash(&self.hasher, from, rel, to), |l| is_link(l, from, rel, to))
    }

    pub fn out_edges(&self, id: &str) -> &[(String, Id)] {
        self.out.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn in_edges(&self, id: &str) -> &[(String, Id)] {
        self.inc.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every step from `id`, both directions.
    pub fn steps<'a>(&'a self, id: &str) -> impl Iterator<Item = Step<'a>> + 'a {
        let out = self.out_edges(id).iter().map(|(r, o)| Step { rel: r, other: o, dir: Dir::Out });
        let inc = self.in_edges(id).iter().map(|(r, o)| Step { rel: r, other: o, dir: Dir::In });
        out.chain(inc)
    }

    /// Steps from `id` along canonical relation `base` in direction `dir`.
    pub fn steps_via<'a>(&'a self, id: &str, base: &str, dir: Dir) -> Vec<Step<'a>> {
        let mut steps = Vec::new();
        if matches!(dir, Dir::Out | Dir::Both) {
            steps.extend(self.out_edges(id).iter().filter(|(r, _)| r == base).map(|(r, o)| Step {
                rel: r,
                other: o,
                dir: Dir::Out,
            }));
        }
        if matches!(dir, Dir::In | Dir::Both) {
            steps.extend(self.in_edges(id).iter().filter(|(r, _)| r == base).map(|(r, o)| Step {
                rel: r,
                other: o,
                dir: Dir::In,
            }));
        }
        steps
    }

    /// Neighbour ids along `base` in `dir`.
    pub fn neighbors(&self, id: &str, base: &str, dir: Dir) -> Vec<&str> {
        self.steps_via(id, base, dir).into_iter().map(|s| s.other).collect()
    }

    pub fn of_kind(&self, kind: &str) -> impl Iterator<Item = &Entity> {
        self.by_kind.get(kind).into_iter().flat_map(|ids| ids.iter()).filter_map(|id| self.get(id))
    }

    pub fn kinds(&self) -> impl Iterator<Item = (&String, usize)> {
        self.by_kind.iter().map(|(k, v)| (k, v.len()))
    }

    /// Exact (normalized) name lookup over ids, keys, titles and aliases.
    pub fn by_name(&self, name: &str) -> Vec<&Entity> {
        self.names
            .get(&normalize(name))
            .into_iter()
            .flat_map(|ids| ids.as_slice())
            .filter_map(|id| self.get(id))
            .collect()
    }

    /// Every name key with the ids it refers to, sorted ascending.
    pub fn names(&self) -> impl Iterator<Item = (&String, &[Id])> {
        self.names.iter().map(|(k, ids)| (k, ids.as_slice()))
    }

    fn index(&mut self, e: &Entity) {
        self.by_kind.entry(e.kind.clone()).or_default().insert(e.id.clone());
        for name in name_keys(e) {
            match self.names.entry(name) {
                Entry::Occupied(mut o) => o.get_mut().insert(&e.id),
                Entry::Vacant(v) => {
                    v.insert(NameIds::One(e.id.clone()));
                }
            }
        }
    }

    fn unindex(&mut self, e: &Entity) {
        if let Some(set) = self.by_kind.get_mut(&e.kind) {
            set.remove(&e.id);
            if set.is_empty() {
                self.by_kind.remove(&e.kind);
            }
        }
        for name in name_keys(e) {
            if let Some(ids) = self.names.get_mut(&name)
                && ids.remove(&e.id)
            {
                self.names.remove(&name);
            }
        }
    }
}

pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Names an entity can be referred to by.
pub fn name_keys(e: &Entity) -> Vec<String> {
    let mut keys = vec![normalize(&e.id)];
    if let Some((kind, key)) = e.id.split_once(':') {
        let key = normalize(key);
        keys.push(format!("{kind} {key}"));
        keys.push(format!("#{key}"));
        keys.push(format!("{kind} #{key}"));
        if key.chars().any(|c| c.is_alphabetic()) || key.len() > 3 {
            keys.push(key.replace(['-', '_'], " "));
            keys.push(key);
        }
    }
    if !e.title.is_empty() {
        let title = normalize(&e.title);
        if let Some(rest) = title.strip_prefix(&format!("{} ", e.kind))
            && !rest.is_empty()
        {
            keys.push(rest.to_string());
        }
        keys.push(title);
    }
    for alias in &e.aliases {
        keys.push(normalize(alias));
    }
    keys.retain(|k| !k.is_empty());
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: &str, title: &str) -> Entity {
        Entity {
            id: id.into(),
            kind: id.split(':').next().unwrap().into(),
            workspace: "w".into(),
            title: title.into(),
            state: None,
            props: Default::default(),
            aliases: vec![],
            provenance: None,
            created_at: 0,
            updated_at: 0,
            revision: 1,
        }
    }

    fn link(from: &str, rel: &str, to: &str) -> Link {
        Link {
            from: from.into(),
            rel: rel.into(),
            to: to.into(),
            props: Default::default(),
            workspace: "w".into(),
            created_at: 0,
        }
    }

    #[test]
    fn indexes_names_and_edges() {
        let mut g = Graph::default();
        g.put_entity(entity("issue:421", "Payment timeout"));
        g.put_entity(entity("project:alpha", "Project Alpha"));
        g.put_link(link("issue:421", "belongs_to", "project:alpha"));
        assert_eq!(g.by_name("#421")[0].id, "issue:421");
        assert_eq!(g.by_name("issue 421")[0].id, "issue:421");
        assert_eq!(g.by_name("alpha")[0].id, "project:alpha");
        assert_eq!(g.by_name("PAYMENT   timeout")[0].id, "issue:421");
        assert_eq!(g.neighbors("project:alpha", "belongs_to", Dir::In), vec!["issue:421"]);
        g.remove_entity("project:alpha");
        assert_eq!(g.link_count(), 0);
        assert!(g.out_edges("issue:421").is_empty());
    }

    fn sorted_edges(edges: &[(String, Id)]) -> Vec<(String, Id)> {
        let mut v = edges.to_vec();
        v.sort();
        v
    }

    #[test]
    fn matches_a_reference_model() {
        use std::collections::BTreeMap;
        let mut rng = fastrand::Rng::with_seed(3);
        let rels = ["belongs_to", "blocked_by", "part_of"];
        let id = |rng: &mut fastrand::Rng| format!("{}:{}", ["issue", "task"][rng.usize(..2)], rng.usize(..150));
        let mut g = Graph::default();
        let mut ents: BTreeMap<Id, Entity> = BTreeMap::new();
        let mut links: BTreeMap<(String, String, String), Link> = BTreeMap::new();
        for step in 0..5_000u64 {
            match rng.u8(..10) {
                0..=3 => {
                    let mut e = entity(&id(&mut rng), "Some title");
                    e.revision = step;
                    g.put_entity(e.clone());
                    ents.insert(e.id.clone(), e);
                }
                4 => {
                    let id = id(&mut rng);
                    let removed = ents.remove(&id);
                    assert_eq!(g.remove_entity(&id).map(|e| e.revision), removed.as_ref().map(|e| e.revision));
                    // Links may name absent entities; only removing an entity drops its links.
                    if removed.is_some() {
                        links.retain(|(f, _, t), _| f != &id && t != &id);
                    }
                }
                5..=7 => {
                    let mut l = link(&id(&mut rng), rels[rng.usize(..rels.len())], &id(&mut rng));
                    l.created_at = step as i64;
                    g.put_link(l.clone());
                    links.insert((l.from.clone(), l.rel.clone(), l.to.clone()), l);
                }
                _ => {
                    let key = (id(&mut rng), rels[rng.usize(..rels.len())].to_string(), id(&mut rng));
                    let got = g.remove_link(&key).map(|l| l.created_at);
                    assert_eq!(got, links.remove(&key).map(|l| l.created_at));
                }
            }
        }
        assert_eq!(g.len(), ents.len());
        assert_eq!(g.link_count(), links.len());
        let mut got: Vec<(&str, u64)> = g.entities().map(|e| (e.id.as_str(), e.revision)).collect();
        got.sort();
        assert_eq!(got, ents.values().map(|e| (e.id.as_str(), e.revision)).collect::<Vec<_>>());
        let mut got: Vec<(&str, &str, &str, i64)> =
            g.links().map(|l| (l.from.as_str(), l.rel.as_str(), l.to.as_str(), l.created_at)).collect();
        got.sort();
        let want: Vec<(&str, &str, &str, i64)> =
            links.values().map(|l| (l.from.as_str(), l.rel.as_str(), l.to.as_str(), l.created_at)).collect();
        assert_eq!(got, want);
        for ty in ["issue", "task"] {
            for n in 0..150 {
                let id = format!("{ty}:{n}");
                assert_eq!(g.get(&id).map(|e| e.revision), ents.get(&id).map(|e| e.revision));
                assert_eq!(g.contains(&id), ents.contains_key(&id));
                let out: Vec<(String, Id)> =
                    links.keys().filter(|(f, _, _)| f == &id).map(|(_, r, t)| (r.clone(), t.clone())).collect();
                let inc: Vec<(String, Id)> =
                    links.keys().filter(|(_, _, t)| t == &id).map(|(f, r, _)| (r.clone(), f.clone())).collect();
                assert_eq!(sorted_edges(g.out_edges(&id)), sorted_edges(&out), "out of {id}");
                assert_eq!(sorted_edges(g.in_edges(&id)), sorted_edges(&inc), "in of {id}");
            }
        }
        for ((f, r, t), l) in &links {
            assert_eq!(g.get_link(f, r, t).map(|x| x.created_at), Some(l.created_at));
        }
        assert!(g.get_link("issue:1", "no_such_rel", "issue:2").is_none());
    }

    #[test]
    fn clones_are_independent() {
        let mut g = Graph::default();
        g.put_entity(entity("issue:1", "Payment timeout"));
        g.put_entity(entity("issue:2", "Login loop"));
        g.put_link(link("issue:1", "blocked_by", "issue:2"));
        let mut c = g.clone();
        c.remove_entity("issue:2");
        c.put_entity(entity("issue:3", "Cache miss"));
        c.put_link(link("issue:3", "blocked_by", "issue:1"));
        assert_eq!(g.len(), 2);
        assert_eq!(g.link_count(), 1);
        assert!(g.get_link("issue:1", "blocked_by", "issue:2").is_some());
        assert!(g.get("issue:3").is_none());
        assert_eq!(g.by_name("login loop")[0].id, "issue:2");
        assert_eq!(c.len(), 2);
        assert!(c.get_link("issue:3", "blocked_by", "issue:1").is_some());
        assert!(c.get_link("issue:1", "blocked_by", "issue:2").is_none());
        assert!(c.by_name("login loop").is_empty());
        assert_eq!(c.by_name("cache miss")[0].id, "issue:3");
    }

    #[test]
    fn name_ids_match_a_sorted_set() {
        let mut rng = fastrand::Rng::with_seed(11);
        for _ in 0..200 {
            let mut ids: Option<NameIds> = None;
            let mut reference: BTreeSet<Id> = BTreeSet::new();
            for _ in 0..rng.usize(1..60) {
                let id = format!("x:{}", rng.usize(..12));
                if rng.u8(..3) > 0 {
                    match &mut ids {
                        Some(ids) => ids.insert(&id),
                        None => ids = Some(NameIds::One(id.clone())),
                    }
                    reference.insert(id);
                } else if let Some(set) = &mut ids {
                    let empty = set.remove(&id);
                    reference.remove(&id);
                    assert_eq!(empty, reference.is_empty());
                    if empty {
                        ids = None;
                    }
                }
                let got: Vec<&Id> = ids.iter().flat_map(|s| s.as_slice()).collect();
                assert_eq!(got, reference.iter().collect::<Vec<_>>());
                if let Some(NameIds::Many(v)) = &ids {
                    assert!(v.len() > 1, "a single id is stored inline");
                }
            }
        }
    }

    #[test]
    fn by_name_matches_a_reference_index() {
        let mut rng = fastrand::Rng::with_seed(5);
        let words = ["payment", "timeout", "alpha", "beta", "login", "cache", "retry"];
        let mut g = Graph::default();
        let mut live: HashMap<Id, Entity> = HashMap::new();
        for step in 0..2_000 {
            let id = format!("{}:{}", ["issue", "task"][rng.usize(..2)], rng.usize(..300));
            if rng.u8(..4) == 0 {
                g.remove_entity(&id);
                live.remove(&id);
                continue;
            }
            let title = (0..rng.usize(1..3)).map(|_| words[rng.usize(..words.len())]).collect::<Vec<_>>().join(" ");
            let mut e = entity(&id, &title);
            e.aliases = (0..rng.usize(..3)).map(|_| words[rng.usize(..words.len())].to_string()).collect();
            e.revision = step;
            g.put_entity(e.clone());
            live.insert(id, e);
        }
        let mut expected: HashMap<String, BTreeSet<Id>> = HashMap::new();
        for e in live.values() {
            for key in name_keys(e) {
                expected.entry(key).or_default().insert(e.id.clone());
            }
        }
        assert_eq!(g.names().count(), expected.len());
        for (key, ids) in &expected {
            let got: Vec<&str> = g.by_name(key).into_iter().map(|e| e.id.as_str()).collect();
            assert_eq!(got, ids.iter().map(String::as_str).collect::<Vec<_>>(), "key {key}");
        }
        for (key, ids) in g.names() {
            assert_eq!(ids.iter().collect::<Vec<_>>(), expected[key].iter().collect::<Vec<_>>());
        }
    }
}
