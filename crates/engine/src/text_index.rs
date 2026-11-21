//! Text indexes: the string values of one property of the nodes with a label (or the
//! relationships of a type), kept in memory per committed state like the point,
//! full-text and vector indexes (see [`IndexCache`]), and used by index seeks for
//! `n.p STARTS WITH s`, `n.p ENDS WITH s` and `n.p CONTAINS s`.
//!
//! Each value is posted under its trigrams (three consecutive characters). A seek for a
//! string of three or more characters takes the entities posted under its rarest
//! trigram; a shorter one takes every indexed entity. Either way the candidates are then
//! checked against their stored values, so a seek returns exactly the matches.

use std::collections::{BTreeMap, HashMap, HashSet};

use graphbuff_core::{Changes, Error, GraphStore, IndexDef, IndexEntity, IndexKind, NodeId, RelId, Result, Value};

use crate::fulltext::for_each_entity;
use crate::index_cache::{Cached, IndexCache};

/// `CREATE TEXT INDEX … OPTIONS {indexConfig: {…}}` takes no settings.
pub fn text_kind(config: &BTreeMap<String, Value>) -> Result<IndexKind> {
    match config.keys().next() {
        Some(k) => Err(Error::Semantic(format!("unknown index setting `{k}` for a text index"))),
        None => Ok(IndexKind::Text),
    }
}

/// `CREATE LOOKUP INDEX … OPTIONS {indexConfig: {…}}` takes no settings.
pub fn lookup_kind(config: &BTreeMap<String, Value>) -> Result<IndexKind> {
    match config.keys().next() {
        Some(k) => Err(Error::Semantic(format!("unknown index setting `{k}` for a lookup index"))),
        None => Ok(IndexKind::Lookup),
    }
}

/// What a text seek looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextMatch {
    StartsWith,
    EndsWith,
    Contains,
}

impl TextMatch {
    fn holds(self, value: &str, needle: &str) -> bool {
        match self {
            TextMatch::StartsWith => value.starts_with(needle),
            TextMatch::EndsWith => value.ends_with(needle),
            TextMatch::Contains => value.contains(needle),
        }
    }
}

/// A trigram, its three characters packed into one number.
type Gram = u64;

/// The distinct trigrams of `s`.
fn grams(s: &str) -> HashSet<Gram> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .windows(3)
        .map(|w| (u64::from(w[0]) << 42) | (u64::from(w[1]) << 21) | u64::from(w[2]))
        .collect()
}

/// The contents of one text index.
#[derive(Clone)]
pub(crate) struct TextIndex {
    def: IndexDef,
    /// The string value of each indexed entity.
    values: HashMap<u64, String>,
    postings: HashMap<Gram, HashSet<u64>>,
}

impl TextIndex {
    /// The indexed entities whose value matches `needle` (`how`), sorted.
    pub(crate) fn find(&self, how: TextMatch, needle: &str) -> Vec<u64> {
        let check = |id: &u64| self.values.get(id).is_some_and(|v| how.holds(v, needle));
        let mut ids: Vec<u64> = if needle.chars().count() >= 3 {
            let mut rarest: Option<&HashSet<u64>> = None;
            for g in grams(needle) {
                match self.postings.get(&g) {
                    None => return Vec::new(),
                    Some(p) if rarest.is_none_or(|r| p.len() < r.len()) => rarest = Some(p),
                    Some(_) => {}
                }
            }
            rarest.map_or_else(Vec::new, |p| p.iter().copied().filter(check).collect())
        } else {
            self.values.keys().copied().filter(check).collect()
        };
        ids.sort_unstable();
        ids
    }

    /// The value of `id` the index holds now: its property, if it is a string and `id`
    /// has the label (or type).
    fn current(&self, store: &dyn GraphStore, id: u64) -> Result<Option<String>> {
        let key = &self.def.properties[0];
        let value = match self.def.entity {
            IndexEntity::Node => {
                let n = NodeId(id);
                if !store.node_exists(n)? || !store.node_has_label(n, &self.def.label)? {
                    return Ok(None);
                }
                store.node_property(n, key)?
            }
            IndexEntity::Relationship => {
                let r = RelId(id);
                if !store.relationship_exists(r)? || store.relationship_type(r)?.as_deref() != Some(self.def.label.as_str())
                {
                    return Ok(None);
                }
                store.relationship_property(r, key)?
            }
        };
        Ok(match value {
            Some(Value::String(s)) => Some(s),
            _ => None,
        })
    }

    fn add(&mut self, id: u64, value: String) {
        for g in grams(&value) {
            self.postings.entry(g).or_default().insert(id);
        }
        self.values.insert(id, value);
    }

    fn remove(&mut self, id: u64) {
        let Some(old) = self.values.remove(&id) else { return };
        for g in grams(&old) {
            if let Some(p) = self.postings.get_mut(&g) {
                p.remove(&id);
                if p.is_empty() {
                    self.postings.remove(&g);
                }
            }
        }
    }
}

impl Cached for TextIndex {
    fn def(&self) -> &IndexDef {
        &self.def
    }

    fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<TextIndex> {
        let mut ix = TextIndex { def: def.clone(), values: HashMap::new(), postings: HashMap::new() };
        for_each_entity(store, def, interrupt, |id| {
            if let Some(v) = ix.current(store, id)? {
                ix.add(id, v);
            }
            Ok(())
        })?;
        Ok(ix)
    }

    fn apply(&mut self, store: &dyn GraphStore, changes: &Changes, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        let changed = match self.def.entity {
            IndexEntity::Node => &changes.nodes,
            IndexEntity::Relationship => &changes.rels,
        };
        for (n, &id) in changed.iter().enumerate() {
            if n % 4096 == 4095 {
                interrupt()?;
            }
            let v = self.current(store, id)?;
            self.remove(id);
            if let Some(v) = v {
                self.add(id, v);
            }
        }
        Ok(())
    }
}

/// The text index contents of a committed state.
pub(crate) type TrigramCache = IndexCache<TextIndex>;

#[cfg(test)]
mod tests {
    use super::*;
    use graphbuff_core::MemoryGraph;

    #[test]
    fn finds_what_brute_force_finds() {
        let def = IndexDef::text("ix", IndexEntity::Node, "L", "p");
        let mut ix = TextIndex::build(&MemoryGraph::new(), &def, &|| Ok(())).unwrap();
        let words = ["", "a", "ab", "abc", "abcabc", "xabcx", "Ábc", "naïve café", "über", "aaa", "aaaa", "bca"];
        for (i, w) in words.iter().enumerate() {
            ix.add(i as u64, (*w).to_string());
        }
        ix.remove(9);
        ix.add(9, "zzz".into());
        for needle in ["", "a", "ab", "abc", "bca", "cab", "bc", "aaa", "é c", "ïve", "zz", "zzz", "q", "über", "Ábc"] {
            for how in [TextMatch::StartsWith, TextMatch::EndsWith, TextMatch::Contains] {
                let want: Vec<u64> = (0..words.len() as u64)
                    .filter(|&i| how.holds(if i == 9 { "zzz" } else { words[i as usize] }, needle))
                    .collect();
                assert_eq!(ix.find(how, needle), want, "{how:?} {needle:?}");
            }
        }
    }
}
