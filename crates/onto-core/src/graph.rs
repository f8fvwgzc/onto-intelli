//! In-memory typed graph with adjacency and name indexes.

use std::collections::{BTreeSet, HashMap};

use crate::model::{Entity, Id, Link};
use crate::schema::Dir;

pub type LinkKey = (Id, String, Id);

#[derive(Default)]
pub struct Graph {
    entities: HashMap<Id, Entity>,
    links: HashMap<LinkKey, Link>,
    out: HashMap<Id, Vec<(String, Id)>>,
    inc: HashMap<Id, Vec<(String, Id)>>,
    by_kind: HashMap<String, BTreeSet<Id>>,
    names: HashMap<String, BTreeSet<Id>>,
}

/// One step from a node: relation, neighbour, and whether the stored edge
/// points away from the node (`Dir::Out`) or towards it (`Dir::In`).
#[derive(Clone, Copy, Debug)]
pub struct Step<'a> {
    pub rel: &'a str,
    pub other: &'a str,
    pub dir: Dir,
}

impl Graph {
    pub fn get(&self, id: &str) -> Option<&Entity> {
        self.entities.get(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entities.contains_key(id)
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
        self.entities.values()
    }

    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.links.values()
    }

    pub fn put_entity(&mut self, e: Entity) {
        if let Some(old) = self.entities.remove(&e.id) {
            self.unindex(&old);
        }
        self.index(&e);
        self.entities.insert(e.id.clone(), e);
    }

    /// Removes the entity and every link touching it.
    pub fn remove_entity(&mut self, id: &str) -> Option<Entity> {
        let e = self.entities.remove(id)?;
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
        let key = (link.from.clone(), link.rel.clone(), link.to.clone());
        if !self.links.contains_key(&key) {
            self.out.entry(key.0.clone()).or_default().push((key.1.clone(), key.2.clone()));
            self.inc.entry(key.2.clone()).or_default().push((key.1.clone(), key.0.clone()));
        }
        self.links.insert(key, link);
    }

    pub fn remove_link(&mut self, key: &LinkKey) -> Option<Link> {
        let link = self.links.remove(key)?;
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
        self.links.get(&(from.to_string(), rel.to_string(), to.to_string()))
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
        self.by_kind.get(kind).into_iter().flat_map(|ids| ids.iter()).filter_map(|id| self.entities.get(id))
    }

    pub fn kinds(&self) -> impl Iterator<Item = (&String, usize)> {
        self.by_kind.iter().map(|(k, v)| (k, v.len()))
    }

    /// Exact (normalized) name lookup over ids, keys, titles and aliases.
    pub fn by_name(&self, name: &str) -> Vec<&Entity> {
        self.names
            .get(&normalize(name))
            .into_iter()
            .flat_map(|ids| ids.iter())
            .filter_map(|id| self.entities.get(id))
            .collect()
    }

    pub fn names(&self) -> impl Iterator<Item = (&String, &BTreeSet<Id>)> {
        self.names.iter()
    }

    fn index(&mut self, e: &Entity) {
        self.by_kind.entry(e.kind.clone()).or_default().insert(e.id.clone());
        for name in name_keys(e) {
            self.names.entry(name).or_default().insert(e.id.clone());
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
            if let Some(set) = self.names.get_mut(&name) {
                set.remove(&e.id);
                if set.is_empty() {
                    self.names.remove(&name);
                }
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
}
