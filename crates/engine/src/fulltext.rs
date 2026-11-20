//! Full-text indexes: `CREATE FULLTEXT INDEX`, `db.index.fulltext.queryNodes` /
//! `queryRelationships` and `db.index.fulltext.listAvailableAnalyzers`.
//!
//! An index splits the string properties it covers into terms with an analyzer (Lucene's
//! analyzers, as Neo4j names them) and keeps an inverted index of them in memory, built
//! on first use and kept up to date with each commit (see [`IndexCache`]). Queries use
//! Lucene's classic syntax (terms, phrases, `AND` / `OR` / `NOT`, `+` / `-`, `field:`,
//! wildcards, fuzzy terms, boosts, grouping) and are scored with BM25, as Lucene scores
//! them; scores are close to Neo4j's but not identical.

use std::collections::{BTreeMap, HashMap, HashSet};

use graphbuff_core::{Analyzer, Changes, Error, GraphStore, IndexDef, IndexEntity, IndexKind, NodeId, RelId, Result, Value};

use crate::index_cache::{Cached, IndexCache};
use crate::procedures::{ProcedureContext, ProcedureRows};

/// The full-text index kind of `indexConfig`: `fulltext.analyzer` (a name from
/// [`Analyzer`], `standard-no-stop-words` by default) and
/// `fulltext.eventually_consistent` (accepted; indexes are always up to date at commit).
pub fn fulltext_kind(config: &BTreeMap<String, Value>) -> Result<IndexKind> {
    let mut analyzer = Analyzer::default();
    for (k, v) in config {
        match (k.as_str(), v) {
            ("fulltext.analyzer", Value::String(name)) => {
                analyzer = Analyzer::parse(name).ok_or_else(|| {
                    let names: Vec<&str> = Analyzer::ALL.iter().map(|a| a.name()).collect();
                    Error::Semantic(format!(
                        "unknown full-text analyzer '{name}'; available: {}",
                        names.join(", ")
                    ))
                })?;
            }
            ("fulltext.eventually_consistent", Value::Bool(_)) => {}
            ("fulltext.analyzer" | "fulltext.eventually_consistent", o) => {
                return Err(Error::Type(format!("`{k}` does not accept {}", o.type_name())));
            }
            (k, _) if k.starts_with("fulltext.") => {
                return Err(Error::Unsupported(format!("full-text index setting `{k}` is not supported")));
            }
            _ => return Err(Error::Semantic(format!("unknown index setting `{k}` for a full-text index"))),
        }
    }
    Ok(IndexKind::Fulltext { analyzer })
}

// ---- analysis -------------------------------------------------------------------------------

/// Lucene's English stop words.
const STOP_WORDS: [&str; 33] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it", "no", "not", "of", "on",
    "or", "such", "that", "the", "their", "then", "there", "these", "they", "this", "to", "was", "will", "with",
];

fn description(a: Analyzer) -> &'static str {
    match a {
        Analyzer::StandardNoStopWords => "Words split at Unicode word boundaries, lowercased. The default.",
        Analyzer::Standard => "Words split at Unicode word boundaries, lowercased, without English stop words.",
        Analyzer::English => {
            "English words: split at Unicode word boundaries, lowercased, without stop words and possessive 's, stemmed (Porter)."
        }
        Analyzer::Simple => "Runs of letters, lowercased.",
        Analyzer::Stop => "Runs of letters, lowercased, without English stop words.",
        Analyzer::Whitespace => "Runs of non-whitespace characters, as written.",
        Analyzer::Keyword => "The whole value as one term, as written.",
    }
}

fn has_stop_words(a: Analyzer) -> bool {
    matches!(a, Analyzer::Standard | Analyzer::English | Analyzer::Stop)
}

fn lowercases(a: Analyzer) -> bool {
    !matches!(a, Analyzer::Whitespace | Analyzer::Keyword)
}

/// Han ideographs and hiragana, which the standard tokenizer makes one term each.
fn is_ideograph(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x309F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF)
}

fn word_char(c: char) -> bool {
    (c.is_alphanumeric() || c == '_') && !is_ideograph(c)
}

/// Whether `j` between `a` and `b` keeps them in one word (Unicode word boundary rules
/// WB6/7 and WB11/12): `don't`, `U.S`, `3.14`, `1,000`.
fn joins(a: char, j: char, b: char) -> bool {
    (a.is_alphabetic() && b.is_alphabetic() && matches!(j, '\'' | '\u{2019}' | '.'))
        || (a.is_numeric() && b.is_numeric() && matches!(j, '.' | ',' | ';' | '\'' | '\u{2019}'))
}

/// The words of `text` at (approximately) Unicode word boundaries, as Lucene's standard
/// tokenizer splits it.
fn standard_words(text: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if is_ideograph(c) {
            out.push(c.to_string());
            i += 1;
            continue;
        }
        if !word_char(c) {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        loop {
            if i < n && word_char(chars[i]) {
                i += 1;
            } else if i + 1 < n && word_char(chars[i + 1]) && joins(chars[i - 1], chars[i], chars[i + 1]) {
                i += 2;
            } else {
                break;
            }
        }
        out.push(chars[start..i].iter().collect());
    }
}

/// The terms `analyzer` makes of `text`, in order.
pub(crate) fn analyze(analyzer: Analyzer, text: &str) -> Vec<String> {
    let mut words = Vec::new();
    match analyzer {
        Analyzer::StandardNoStopWords | Analyzer::Standard | Analyzer::English => standard_words(text, &mut words),
        Analyzer::Simple | Analyzer::Stop => {
            words.extend(text.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()).map(String::from));
        }
        Analyzer::Whitespace => words.extend(text.split_whitespace().map(String::from)),
        Analyzer::Keyword => {
            if !text.is_empty() {
                words.push(text.to_string());
            }
        }
    }
    if analyzer == Analyzer::English {
        for w in &mut words {
            for s in ["'s", "'S", "\u{2019}s", "\u{2019}S"] {
                if w.len() > s.len() && w.ends_with(s) {
                    w.truncate(w.len() - s.len());
                    break;
                }
            }
        }
    }
    if lowercases(analyzer) {
        for w in &mut words {
            *w = w.to_lowercase();
        }
    }
    if has_stop_words(analyzer) {
        words.retain(|w| !STOP_WORDS.contains(&w.as_str()));
    }
    if analyzer == Analyzer::English {
        for w in &mut words {
            *w = porter(w);
        }
    }
    words
}

/// A wildcard or fuzzy term as the index keeps terms: lowercased if the analyzer
/// lowercases (but not split or stemmed).
fn normalize(analyzer: Analyzer, term: &str) -> String {
    if lowercases(analyzer) { term.to_lowercase() } else { term.to_string() }
}

/// The Porter stem of `word` (Porter 1980, as Lucene's `PorterStemFilter` applies it).
/// Words of one or two letters, and words that are not plain ASCII, are left as they are.
fn porter(word: &str) -> String {
    if word.len() <= 2 || !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return word.to_string();
    }
    let mut s = Porter { b: word.as_bytes().to_vec(), k: word.len() as isize - 1, j: 0 };
    s.step1ab();
    if s.k > 0 {
        s.step1c();
        s.step2();
        s.step3();
        s.step4();
        s.step5();
    }
    s.b.truncate((s.k + 1) as usize);
    String::from_utf8(s.b).expect("ASCII")
}

struct Porter {
    b: Vec<u8>,
    /// The end of the word (inclusive).
    k: isize,
    /// The end of the stem a suffix test left.
    j: isize,
}

impl Porter {
    fn at(&self, i: isize) -> u8 {
        self.b[i as usize]
    }

    fn cons(&self, i: isize) -> bool {
        match self.at(i) {
            b'a' | b'e' | b'i' | b'o' | b'u' => false,
            b'y' => i == 0 || !self.cons(i - 1),
            _ => true,
        }
    }

    /// The number of consonant sequences between 0 and `j`.
    fn m(&self) -> usize {
        let (mut n, mut i) = (0, 0);
        loop {
            if i > self.j {
                return n;
            }
            if !self.cons(i) {
                break;
            }
            i += 1;
        }
        i += 1;
        loop {
            loop {
                if i > self.j {
                    return n;
                }
                if self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
            n += 1;
            loop {
                if i > self.j {
                    return n;
                }
                if !self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
    }

    fn vowel_in_stem(&self) -> bool {
        (0..=self.j).any(|i| !self.cons(i))
    }

    fn double_c(&self, j: isize) -> bool {
        j >= 1 && self.at(j) == self.at(j - 1) && self.cons(j)
    }

    /// Consonant-vowel-consonant ending at `i`, the last not w, x or y.
    fn cvc(&self, i: isize) -> bool {
        if i < 2 || !self.cons(i) || self.cons(i - 1) || !self.cons(i - 2) {
            return false;
        }
        !matches!(self.at(i), b'w' | b'x' | b'y')
    }

    fn ends(&mut self, s: &str) -> bool {
        let len = s.len() as isize;
        if len > self.k + 1 || &self.b[(self.k + 1 - len) as usize..=self.k as usize] != s.as_bytes() {
            return false;
        }
        self.j = self.k - len;
        true
    }

    fn set_to(&mut self, s: &str) {
        let at = (self.j + 1) as usize;
        self.b.truncate(at);
        self.b.extend_from_slice(s.as_bytes());
        self.k = self.j + s.len() as isize;
    }

    fn r(&mut self, s: &str) {
        if self.m() > 0 {
            self.set_to(s);
        }
    }

    fn step1ab(&mut self) {
        if self.at(self.k) == b's' {
            if self.ends("sses") {
                self.k -= 2;
            } else if self.ends("ies") {
                self.set_to("i");
            } else if self.at(self.k - 1) != b's' {
                self.k -= 1;
            }
        }
        if self.ends("eed") {
            if self.m() > 0 {
                self.k -= 1;
            }
        } else if (self.ends("ed") || self.ends("ing")) && self.vowel_in_stem() {
            self.k = self.j;
            if self.ends("at") {
                self.set_to("ate");
            } else if self.ends("bl") {
                self.set_to("ble");
            } else if self.ends("iz") {
                self.set_to("ize");
            } else if self.double_c(self.k) {
                self.k -= 1;
                if matches!(self.at(self.k), b'l' | b's' | b'z') {
                    self.k += 1;
                }
            } else if self.m() == 1 && self.cvc(self.k) {
                self.set_to("e");
            }
        }
    }

    fn step1c(&mut self) {
        if self.ends("y") && self.vowel_in_stem() {
            let k = self.k as usize;
            self.b[k] = b'i';
        }
    }

    /// Replaces the first of `rules` (suffix, replacement) the word ends with, if its
    /// stem has a measure above 0.
    fn first(&mut self, rules: &[(&str, &str)]) {
        for (suffix, to) in rules {
            if self.ends(suffix) {
                self.r(to);
                return;
            }
        }
    }

    fn step2(&mut self) {
        match self.at(self.k - 1) {
            b'a' => self.first(&[("ational", "ate"), ("tional", "tion")]),
            b'c' => self.first(&[("enci", "ence"), ("anci", "ance")]),
            b'e' => self.first(&[("izer", "ize")]),
            b'l' => self.first(&[("bli", "ble"), ("alli", "al"), ("entli", "ent"), ("eli", "e"), ("ousli", "ous")]),
            b'o' => self.first(&[("ization", "ize"), ("ation", "ate"), ("ator", "ate")]),
            b's' => self.first(&[("alism", "al"), ("iveness", "ive"), ("fulness", "ful"), ("ousness", "ous")]),
            b't' => self.first(&[("aliti", "al"), ("iviti", "ive"), ("biliti", "ble")]),
            b'g' => self.first(&[("logi", "log")]),
            _ => {}
        }
    }

    fn step3(&mut self) {
        match self.at(self.k) {
            b'e' => self.first(&[("icate", "ic"), ("ative", ""), ("alize", "al")]),
            b'i' => self.first(&[("iciti", "ic")]),
            b'l' => self.first(&[("ical", "ic"), ("ful", "")]),
            b's' => self.first(&[("ness", "")]),
            _ => {}
        }
    }

    fn step4(&mut self) {
        let suffixes: &[&str] = match self.at(self.k - 1) {
            b'a' => &["al"],
            b'c' => &["ance", "ence"],
            b'e' => &["er"],
            b'i' => &["ic"],
            b'l' => &["able", "ible"],
            b'n' => &["ant", "ement", "ment", "ent"],
            b'o' => {
                if self.ends("ion") && self.j >= 0 && matches!(self.at(self.j), b's' | b't') {
                    if self.m() > 1 {
                        self.k = self.j;
                    }
                    return;
                }
                &["ou"]
            }
            b's' => &["ism"],
            b't' => &["ate", "iti"],
            b'u' => &["ous"],
            b'v' => &["ive"],
            b'z' => &["ize"],
            _ => return,
        };
        if suffixes.iter().any(|s| self.ends(s)) && self.m() > 1 {
            self.k = self.j;
        }
    }

    fn step5(&mut self) {
        self.j = self.k;
        if self.at(self.k) == b'e' {
            let a = self.m();
            if a > 1 || (a == 1 && !self.cvc(self.k - 1)) {
                self.k -= 1;
            }
        }
        if self.at(self.k) == b'l' && self.double_c(self.k) && self.m() > 1 {
            self.k -= 1;
        }
    }
}

// ---- the inverted index ---------------------------------------------------------------------

/// A full-text index's terms in one committed state.
#[derive(Clone)]
pub(crate) struct TextIndex {
    def: IndexDef,
    analyzer: Analyzer,
    /// The id of each term.
    terms: BTreeMap<String, u32>,
    /// Each term (empty for an id not in use).
    names: Vec<String>,
    /// Per term: how often it occurs in each (entity, field).
    postings: Vec<HashMap<(u64, u16), u32>>,
    /// Term ids not in use.
    free: Vec<u32>,
    /// Per indexed entity: per field (property), its terms in order.
    docs: HashMap<u64, Vec<Vec<u32>>>,
    /// Per field: the entities with terms in it, and their terms in all.
    field_docs: Vec<u64>,
    field_terms: Vec<u64>,
}

fn analyzer_of(def: &IndexDef) -> Analyzer {
    match def.kind {
        IndexKind::Fulltext { analyzer } => analyzer,
        _ => unreachable!("a full-text index"),
    }
}

/// The terms of a property value: those of a string, or of each string of a list.
fn value_terms(analyzer: Analyzer, value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => analyze(analyzer, s),
        Value::List(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(analyze(analyzer, s)),
                _ => None,
            })
            .flatten()
            .collect(),
        _ => Vec::new(),
    }
}

/// Calls `f` with the id of every entity `def` covers in `store` (nodes with one of its
/// labels, or relationships of one of its types).
fn for_each_entity(
    store: &dyn GraphStore,
    def: &IndexDef,
    interrupt: &dyn Fn() -> Result<()>,
    mut f: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    let mut n = 0u32;
    let mut tick = || {
        n = n.wrapping_add(1);
        if n.is_multiple_of(4096) { interrupt() } else { Ok(()) }
    };
    match def.entity {
        IndexEntity::Node => {
            let mut seen = HashSet::new();
            let several = !def.extra_labels.is_empty();
            for label in def.labels() {
                for id in store.nodes_with_label(label) {
                    let id = id?;
                    tick()?;
                    if !several || seen.insert(id.0) {
                        f(id.0)?;
                    }
                }
            }
        }
        IndexEntity::Relationship => {
            let types: HashSet<&str> = def.labels().collect();
            for id in store.all_relationships() {
                let id = id?;
                tick()?;
                if store.relationship_type(id)?.is_some_and(|t| types.contains(t.as_str())) {
                    f(id.0)?;
                }
            }
        }
    }
    Ok(())
}

impl TextIndex {
    fn empty(def: &IndexDef) -> TextIndex {
        let fields = def.properties.len();
        TextIndex {
            def: def.clone(),
            analyzer: analyzer_of(def),
            terms: BTreeMap::new(),
            names: Vec::new(),
            postings: Vec::new(),
            free: Vec::new(),
            docs: HashMap::new(),
            field_docs: vec![0; fields],
            field_terms: vec![0; fields],
        }
    }

    /// The terms of each field of entity `id` in `store`, or `None` if the index does not
    /// cover it (or it has no terms).
    fn current(&self, store: &dyn GraphStore, id: u64) -> Result<Option<Vec<Vec<String>>>> {
        let covered = match self.def.entity {
            IndexEntity::Node => {
                let n = NodeId(id);
                if !store.node_exists(n)? {
                    false
                } else {
                    let mut any = false;
                    for l in self.def.labels() {
                        if store.node_has_label(n, l)? {
                            any = true;
                            break;
                        }
                    }
                    any
                }
            }
            IndexEntity::Relationship => {
                let r = RelId(id);
                store.relationship_exists(r)?
                    && store.relationship_type(r)?.is_some_and(|t| self.def.labels().any(|l| l == t))
            }
        };
        if !covered {
            return Ok(None);
        }
        let mut fields = Vec::with_capacity(self.def.properties.len());
        for key in &self.def.properties {
            let value = match self.def.entity {
                IndexEntity::Node => store.node_property(NodeId(id), key)?,
                IndexEntity::Relationship => store.relationship_property(RelId(id), key)?,
            };
            fields.push(value.map(|v| value_terms(self.analyzer, &v)).unwrap_or_default());
        }
        Ok(if fields.iter().all(Vec::is_empty) { None } else { Some(fields) })
    }

    fn term_id(&mut self, term: String) -> u32 {
        if let Some(&id) = self.terms.get(&term) {
            return id;
        }
        let id = match self.free.pop() {
            Some(id) => {
                self.names[id as usize] = term.clone();
                id
            }
            None => {
                self.names.push(term.clone());
                self.postings.push(HashMap::new());
                (self.names.len() - 1) as u32
            }
        };
        self.terms.insert(term, id);
        id
    }

    fn add(&mut self, id: u64, fields: Vec<Vec<String>>) {
        let mut doc = Vec::with_capacity(fields.len());
        for (f, terms) in fields.into_iter().enumerate() {
            if !terms.is_empty() {
                self.field_docs[f] += 1;
                self.field_terms[f] += terms.len() as u64;
            }
            let ids: Vec<u32> = terms.into_iter().map(|t| self.term_id(t)).collect();
            for &t in &ids {
                *self.postings[t as usize].entry((id, f as u16)).or_insert(0) += 1;
            }
            doc.push(ids);
        }
        self.docs.insert(id, doc);
    }

    fn remove(&mut self, id: u64) {
        let Some(doc) = self.docs.remove(&id) else { return };
        for (f, ids) in doc.into_iter().enumerate() {
            if !ids.is_empty() {
                self.field_docs[f] -= 1;
                self.field_terms[f] -= ids.len() as u64;
            }
            for t in ids {
                let p = &mut self.postings[t as usize];
                if p.remove(&(id, f as u16)).is_some() && p.is_empty() {
                    let name = std::mem::take(&mut self.names[t as usize]);
                    self.terms.remove(&name);
                    p.shrink_to_fit();
                    self.free.push(t);
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
        let mut ix = TextIndex::empty(def);
        for_each_entity(store, def, interrupt, |id| {
            if let Some(fields) = ix.current(store, id)? {
                ix.add(id, fields);
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
            let fields = self.current(store, id)?;
            self.remove(id);
            if let Some(fields) = fields {
                self.add(id, fields);
            }
        }
        Ok(())
    }
}

/// The full-text index contents of a committed state.
pub(crate) type TextCache = IndexCache<TextIndex>;

// ---- queries --------------------------------------------------------------------------------

/// How a clause of a boolean query takes part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Occur {
    Must,
    Should,
    MustNot,
}

/// A parsed query. `field` is a property name, or `None` for all of the index's.
#[derive(Clone, Debug, PartialEq)]
enum Query {
    /// Text analyzed into terms; any of them matches.
    Term { field: Option<String>, text: String },
    /// Text analyzed into terms that must follow each other.
    Phrase { field: Option<String>, text: String },
    /// A term with `*` (any characters) and `?` (one character).
    Wildcard { field: Option<String>, pattern: String },
    /// Terms within `edits` edits of `term`.
    Fuzzy { field: Option<String>, term: String, edits: usize },
    /// `*:*`: every indexed entity.
    All,
    Bool(Vec<(Occur, Query)>),
    Boost(Box<Query>, f64),
}

fn syntax(query: &str, why: impl std::fmt::Display) -> Error {
    Error::Semantic(format!("cannot parse full-text query '{query}': {why}"))
}

/// A parser of Lucene's classic query syntax.
struct QueryParser<'a> {
    text: &'a str,
    s: Vec<char>,
    i: usize,
}

/// Characters that end a term unless escaped.
fn special(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '^' | '"' | '~' | ':' | '\\' | '/')
}

impl QueryParser<'_> {
    fn parse(text: &str) -> Result<Query> {
        let mut p = QueryParser { text, s: text.chars().collect(), i: 0 };
        let q = p.boolean(None, false)?;
        if p.i < p.s.len() {
            return Err(p.err(format!("unexpected '{}'", p.s[p.i])));
        }
        match &q {
            Query::Bool(c) if c.is_empty() => Err(p.err("the query is empty")),
            _ => Ok(q),
        }
    }

    fn err(&self, why: impl std::fmt::Display) -> Error {
        syntax(self.text, why)
    }

    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
    }

    /// Consumes operator `word` (`AND`, `OR`, `NOT`, `&&`, `||`) if it stands alone here.
    fn eat_op(&mut self, word: &str) -> bool {
        let w: Vec<char> = word.chars().collect();
        if self.s.len() < self.i + w.len() || self.s[self.i..self.i + w.len()] != w[..] {
            return false;
        }
        let next = self.s.get(self.i + w.len()).copied();
        let alone = word.starts_with(['&', '|']) || next.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '"'));
        if alone {
            self.i += w.len();
        }
        alone
    }

    /// Clauses up to the end (or, `nested`, a `)`), joined as Lucene's classic parser
    /// joins them with the default operator `OR`.
    fn boolean(&mut self, field: Option<&str>, nested: bool) -> Result<Query> {
        let mut clauses: Vec<(Occur, Query)> = Vec::new();
        let mut and = false;
        let mut conj = false;
        loop {
            self.skip_ws();
            match self.peek() {
                None => break,
                Some(')') if nested => break,
                _ => {}
            }
            if self.eat_op("AND") || self.eat_op("&&") || self.eat_op("OR") || self.eat_op("||") {
                let is_and = matches!(self.s[self.i - 1], 'D' | '&');
                if clauses.is_empty() || conj {
                    return Err(self.err("AND / OR needs a clause on each side"));
                }
                and = is_and;
                conj = true;
                continue;
            }
            let modifier = if self.eat_op("NOT") {
                Some(Occur::MustNot)
            } else {
                match self.peek() {
                    Some('!') => Some(Occur::MustNot),
                    Some('-') => Some(Occur::MustNot),
                    Some('+') => Some(Occur::Must),
                    _ => None,
                }
                .inspect(|_| self.i += 1)
            };
            self.skip_ws();
            let clause = self.clause(field)?;
            if and
                && let Some(last) = clauses.last_mut()
                && last.0 != Occur::MustNot
            {
                last.0 = Occur::Must;
            }
            let occur = modifier.unwrap_or(if and { Occur::Must } else { Occur::Should });
            clauses.push((occur, clause));
            and = false;
            conj = false;
        }
        if conj {
            return Err(self.err("AND / OR needs a clause on each side"));
        }
        Ok(Query::Bool(clauses))
    }

    /// A term's characters (escapes resolved), and whether it has unescaped wildcards.
    fn term(&mut self) -> (String, bool) {
        let (mut out, mut wild) = (String::new(), false);
        while let Some(c) = self.peek() {
            if c == '\\' {
                if let Some(&e) = self.s.get(self.i + 1) {
                    out.push(e);
                    self.i += 2;
                    continue;
                }
                break;
            }
            if special(c) || (out.is_empty() && matches!(c, '+' | '-' | '!')) {
                break;
            }
            if matches!(c, '*' | '?') {
                wild = true;
            }
            out.push(c);
            self.i += 1;
        }
        (out, wild)
    }

    /// A number here, as written.
    fn number_text(&mut self) -> String {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '.') {
            self.i += 1;
        }
        self.s[start..self.i].iter().collect()
    }

    fn number(&mut self) -> Option<f64> {
        self.number_text().parse().ok()
    }

    fn clause(&mut self, field: Option<&str>) -> Result<Query> {
        let start = self.i;
        let (mut text, mut wild) = self.term();
        let mut field = field.map(String::from);
        if !text.is_empty() && self.peek() == Some(':') {
            self.i += 1;
            field = Some(text);
            self.skip_ws();
            (text, wild) = self.term();
        }
        let q = if !text.is_empty() {
            if field.as_deref() == Some("*") && text == "*" {
                Query::All
            } else if self.peek() == Some('~') {
                self.i += 1;
                let n = self.number_text();
                let edits = if n.is_empty() {
                    2
                } else if let Ok(edits) = n.parse::<usize>() {
                    edits.min(2)
                } else {
                    // The older similarity form, `term~0.8`.
                    let similarity: f64 = n.parse().map_err(|_| self.err(format!("bad fuzzy edit distance '{n}'")))?;
                    if !(0.0..1.0).contains(&similarity) {
                        return Err(self.err(format!("bad fuzzy edit distance '{n}'")));
                    }
                    (((1.0 - similarity) * text.chars().count() as f64).floor() as usize).min(2)
                };
                Query::Fuzzy { field, term: text, edits }
            } else if wild {
                Query::Wildcard { field, pattern: text }
            } else {
                Query::Term { field, text }
            }
        } else {
            match self.peek() {
                Some('(') => {
                    self.i += 1;
                    let q = self.boolean(field.as_deref(), true)?;
                    if self.peek() != Some(')') {
                        return Err(self.err("a '(' is not closed"));
                    }
                    self.i += 1;
                    q
                }
                Some('"') => {
                    self.i += 1;
                    let mut phrase = String::new();
                    loop {
                        match self.peek() {
                            None => return Err(self.err("a '\"' is not closed")),
                            Some('"') => break,
                            Some('\\') if self.i + 1 < self.s.len() => {
                                phrase.push(self.s[self.i + 1]);
                                self.i += 2;
                            }
                            Some(c) => {
                                phrase.push(c);
                                self.i += 1;
                            }
                        }
                    }
                    self.i += 1;
                    if self.peek() == Some('~') {
                        return Err(Error::Unsupported(
                            "full-text queries do not support phrase slop (\"…\"~n)".into(),
                        ));
                    }
                    Query::Phrase { field, text: phrase }
                }
                Some('[' | '{') => {
                    return Err(Error::Unsupported("full-text queries do not support ranges ([a TO b])".into()));
                }
                Some('/') => {
                    return Err(Error::Unsupported("full-text queries do not support regular expressions".into()));
                }
                Some(c) => return Err(self.err(format!("unexpected '{c}'"))),
                None if self.i > start => return Err(self.err("a field needs a term")),
                None => return Err(self.err("expected a term")),
            }
        };
        if self.peek() == Some('^') {
            self.i += 1;
            let boost = self.number().ok_or_else(|| self.err("'^' needs a number"))?;
            return Ok(Query::Boost(Box::new(q), boost));
        }
        Ok(q)
    }
}

/// Whether `text` matches `pattern` (`*` any characters, `?` one).
fn wildcard_match(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// The edit distance of `a` and `b` (insertions, deletions, substitutions and
/// transpositions of neighbours), or `None` if above `max`.
fn edit_distance(a: &[char], b: &[char], max: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev2: Vec<usize> = Vec::new();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        let mut low = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
            low = low.min(cur[j]);
        }
        if low > max {
            return None;
        }
        prev2 = std::mem::replace(&mut prev, cur);
    }
    (prev[b.len()] <= max).then_some(prev[b.len()])
}

/// BM25 parameters, Lucene's defaults.
const K1: f64 = 1.2;
const B: f64 = 0.75;

type Scores = HashMap<u64, f64>;

fn add_into(into: &mut Scores, from: Scores) {
    for (id, s) in from {
        *into.entry(id).or_insert(0.0) += s;
    }
}

/// Runs queries against a [`TextIndex`].
struct Searcher<'a> {
    ix: &'a TextIndex,
    analyzer: Analyzer,
    interrupt: &'a dyn Fn() -> Result<()>,
}

impl Searcher<'_> {
    /// The fields `field` names: all of the index's for `None`; none for a property the
    /// index does not cover.
    fn fields(&self, field: &Option<String>) -> Vec<u16> {
        match field {
            None => (0..self.ix.def.properties.len() as u16).collect(),
            Some(f) => self.ix.def.properties.iter().position(|p| p == f).map(|i| i as u16).into_iter().collect(),
        }
    }

    /// The entities with term `t` in field `f`, and how often.
    fn postings(&self, t: u32, f: u16) -> Vec<(u64, u32)> {
        self.ix.postings[t as usize].iter().filter(|((_, ff), _)| *ff == f).map(|((id, _), n)| (*id, *n)).collect()
    }

    fn idf(&self, f: u16, df: usize) -> f64 {
        let n = self.ix.field_docs[f as usize] as f64;
        let df = df as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// The BM25 score of `freq` occurrences in field `f` of entity `id`, of terms with
    /// inverse document frequency `idf`.
    fn bm25(&self, id: u64, f: u16, freq: u32, idf: f64) -> f64 {
        let len = self.ix.docs[&id][f as usize].len() as f64;
        let avg = self.ix.field_terms[f as usize] as f64 / self.ix.field_docs[f as usize].max(1) as f64;
        let freq = f64::from(freq);
        idf * freq / (freq + K1 * (1.0 - B + B * len / avg.max(f64::MIN_POSITIVE)))
    }

    fn term_scores(&self, term: &str, fields: &[u16], weight: f64, into: &mut Scores) {
        let Some(&t) = self.ix.terms.get(term) else { return };
        for &f in fields {
            let docs = self.postings(t, f);
            let idf = self.idf(f, docs.len());
            for (id, n) in docs {
                *into.entry(id).or_insert(0.0) += weight * self.bm25(id, f, n, idf);
            }
        }
    }

    /// The ids of the terms `matches` accepts, from those starting with `prefix`.
    fn expand(&self, prefix: &str, mut matches: impl FnMut(&str) -> Option<f64>) -> Result<Vec<(u32, f64)>> {
        let mut out = Vec::new();
        for (n, (term, &t)) in self.ix.terms.range(prefix.to_string()..).enumerate() {
            if !term.starts_with(prefix) {
                break;
            }
            if n % 4096 == 4095 {
                (self.interrupt)()?;
            }
            if let Some(w) = matches(term) {
                out.push((t, w));
            }
        }
        Ok(out)
    }

    /// The scores of the entities `q` matches; `None` if it analyzed to nothing (a clause
    /// of stop words), which drops it from a boolean query.
    fn eval(&self, q: &Query) -> Result<Option<Scores>> {
        (self.interrupt)()?;
        let mut out = Scores::new();
        match q {
            Query::Term { field, text } => {
                let terms = analyze(self.analyzer, text);
                if terms.is_empty() {
                    return Ok(None);
                }
                let fields = self.fields(field);
                for t in &terms {
                    self.term_scores(t, &fields, 1.0, &mut out);
                }
            }
            Query::Phrase { field, text } => {
                let terms = analyze(self.analyzer, text);
                match terms.len() {
                    0 => return Ok(None),
                    1 => self.term_scores(&terms[0], &self.fields(field), 1.0, &mut out),
                    _ => {
                        let Some(ids) = terms.iter().map(|t| self.ix.terms.get(t).copied()).collect::<Option<Vec<u32>>>()
                        else {
                            return Ok(Some(out));
                        };
                        for f in self.fields(field) {
                            let idf: f64 = ids.iter().map(|&t| self.idf(f, self.postings(t, f).len())).sum();
                            for (id, _) in self.postings(ids[0], f) {
                                let doc = &self.ix.docs[&id][f as usize];
                                let freq = doc.windows(ids.len()).filter(|w| *w == &ids[..]).count() as u32;
                                if freq > 0 {
                                    *out.entry(id).or_insert(0.0) += self.bm25(id, f, freq, idf);
                                }
                            }
                        }
                    }
                }
            }
            Query::Wildcard { field, pattern } => {
                let pattern: Vec<char> = normalize(self.analyzer, pattern).chars().collect();
                let prefix: String = pattern.iter().take_while(|c| !matches!(c, '*' | '?')).collect();
                let terms = self.expand(&prefix, |term| {
                    let chars: Vec<char> = term.chars().collect();
                    wildcard_match(&pattern, &chars).then_some(1.0)
                })?;
                // Constant scores, as Lucene rewrites wildcard queries.
                for f in self.fields(field) {
                    let mut hit = HashSet::new();
                    for &(t, _) in &terms {
                        hit.extend(self.postings(t, f).into_iter().map(|(id, _)| id));
                    }
                    for id in hit {
                        *out.entry(id).or_insert(0.0) += 1.0;
                    }
                }
            }
            Query::Fuzzy { field, term, edits } => {
                let q: Vec<char> = normalize(self.analyzer, term).chars().collect();
                let terms = self.expand("", |term| {
                    let chars: Vec<char> = term.chars().collect();
                    let d = edit_distance(&q, &chars, *edits)?;
                    let shorter = q.len().min(chars.len());
                    (shorter > 0).then(|| 1.0 - d as f64 / shorter as f64).filter(|w| *w > 0.0)
                })?;
                let fields = self.fields(field);
                for (t, w) in terms {
                    let name = &self.ix.names[t as usize];
                    self.term_scores(name, &fields, w, &mut out);
                }
            }
            Query::All => {
                for &id in self.ix.docs.keys() {
                    out.insert(id, 1.0);
                }
            }
            Query::Boost(q, b) => {
                let Some(mut s) = self.eval(q)? else { return Ok(None) };
                for v in s.values_mut() {
                    *v *= b;
                }
                return Ok(Some(s));
            }
            Query::Bool(clauses) => {
                let (mut must, mut should, mut not) = (Vec::new(), Vec::new(), Vec::new());
                for (occur, c) in clauses {
                    let Some(s) = self.eval(c)? else { continue };
                    match occur {
                        Occur::Must => must.push(s),
                        Occur::Should => should.push(s),
                        Occur::MustNot => not.push(s),
                    }
                }
                if must.is_empty() && should.is_empty() {
                    // Only negative clauses match nothing; no clauses left drops this one.
                    return Ok((!not.is_empty()).then_some(out));
                }
                if must.is_empty() {
                    for s in should {
                        add_into(&mut out, s);
                    }
                } else {
                    let mut must = must.into_iter();
                    out = must.next().expect("not empty");
                    for s in must {
                        out.retain(|id, _| s.contains_key(id));
                        for (id, v) in out.iter_mut() {
                            *v += s[id];
                        }
                    }
                    for s in should {
                        for (id, v) in out.iter_mut() {
                            *v += s.get(id).copied().unwrap_or(0.0);
                        }
                    }
                }
                for s in not {
                    out.retain(|id, _| !s.contains_key(id));
                }
            }
        }
        Ok(Some(out))
    }
}

/// The entities of `ix` that `query` matches, with their scores, best first (ties by id).
fn search(ix: &TextIndex, analyzer: Analyzer, query: &str, interrupt: &dyn Fn() -> Result<()>) -> Result<Vec<(u64, f64)>> {
    let q = QueryParser::parse(query)?;
    let scores = Searcher { ix, analyzer, interrupt }.eval(&q)?.unwrap_or_default();
    let mut hits: Vec<(u64, f64)> = scores.into_iter().collect();
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(hits)
}

/// `db.index.fulltext.queryNodes(indexName, queryString, options)` (`entity` Node) and
/// `db.index.fulltext.queryRelationships(…)`: the indexed entities matching the query, as
/// `[entity, score]` rows, best first. `options` may hold `skip`, `limit` and
/// `analyzer` (to analyze the query with another analyzer than the index's).
pub fn query_index(ctx: &ProcedureContext<'_>, entity: IndexEntity, args: &[Value]) -> Result<ProcedureRows> {
    let procedure = match entity {
        IndexEntity::Node => "db.index.fulltext.queryNodes",
        IndexEntity::Relationship => "db.index.fulltext.queryRelationships",
    };
    let name = match &args[0] {
        Value::String(s) => s.clone(),
        o => return Err(Error::Type(format!("{procedure}: indexName must be a STRING, not {}", o.type_name()))),
    };
    let query = match &args[1] {
        Value::String(s) => s.clone(),
        o => return Err(Error::Type(format!("{procedure}: queryString must be a STRING, not {}", o.type_name()))),
    };
    let Some(def) = ctx.store().indexes().into_iter().find(|d| d.name == name && d.is_fulltext()) else {
        return Err(Error::Semantic(format!("{procedure}: there is no full-text index named `{name}`")));
    };
    if def.entity != entity {
        let other = match def.entity {
            IndexEntity::Node => "db.index.fulltext.queryNodes",
            IndexEntity::Relationship => "db.index.fulltext.queryRelationships",
        };
        return Err(Error::Semantic(format!(
            "{procedure}: `{name}` indexes {}s; use {other}",
            def.entity.text().to_lowercase()
        )));
    }
    let (mut skip, mut limit, mut analyzer) = (0usize, None, analyzer_of(&def));
    match args.get(2) {
        None | Some(Value::Null) => {}
        Some(Value::Map(m)) => {
            for (k, v) in m {
                match (k.as_str(), v) {
                    ("skip", Value::Int(n)) if *n >= 0 => skip = *n as usize,
                    ("limit", Value::Int(n)) if *n >= 0 => limit = Some(*n as usize),
                    ("skip" | "limit", o) => {
                        return Err(Error::Semantic(format!("{procedure}: `{k}` must be a non-negative INTEGER, not {o}")));
                    }
                    ("analyzer", Value::String(a)) => {
                        analyzer = Analyzer::parse(a)
                            .ok_or_else(|| Error::Semantic(format!("{procedure}: unknown analyzer '{a}'")))?;
                    }
                    (_, Value::Null) => {}
                    _ => return Err(Error::Semantic(format!("{procedure}: unknown option `{k}`"))),
                }
            }
        }
        Some(o) => return Err(Error::Type(format!("{procedure}: options must be a MAP, not {}", o.type_name()))),
    }
    let interrupt = || ctx.check_interrupt();
    let ix = match ctx.texts() {
        // A committed state: the cached index.
        Some((cache, snapshot)) => cache.get(snapshot, &def, &interrupt)?,
        // Inside a transaction (or writing): built from the entities as the query sees them.
        None => std::sync::Arc::new(TextIndex::build(ctx.store(), &def, &interrupt)?),
    };
    let hits = search(&ix, analyzer, &query, &interrupt)?;
    Ok(hits
        .into_iter()
        .skip(skip)
        .take(limit.unwrap_or(usize::MAX))
        .map(|(id, score)| {
            let e = match entity {
                IndexEntity::Node => Value::Node(NodeId(id)),
                IndexEntity::Relationship => Value::Rel(RelId(id)),
            };
            vec![e, Value::Float(score)]
        })
        .collect())
}

/// `db.index.fulltext.listAvailableAnalyzers()`: `[analyzer, description, stopwords]` rows.
pub fn list_analyzers() -> ProcedureRows {
    Analyzer::ALL
        .into_iter()
        .map(|a| {
            let stop = if has_stop_words(a) { STOP_WORDS.to_vec() } else { Vec::new() };
            vec![
                Value::String(a.name().into()),
                Value::String(description(a).into()),
                Value::List(stop.into_iter().map(|w| Value::String(w.into())).collect()),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(a: Analyzer, s: &str) -> Vec<String> {
        analyze(a, s)
    }

    #[test]
    fn analyzers() {
        use Analyzer::*;
        let s = "The Quick-brown fox's 3.14 don't U.S.A. naïve 東京 e-mail_x";
        assert_eq!(
            terms(StandardNoStopWords, s),
            ["the", "quick", "brown", "fox's", "3.14", "don't", "u.s.a", "naïve", "東", "京", "e", "mail_x"]
        );
        assert_eq!(terms(Standard, "The cat is on the mat"), ["cat", "mat"]);
        assert_eq!(terms(English, "The fox's running horses"), ["fox", "run", "hors"]);
        assert_eq!(terms(Simple, "It's 4you"), ["it", "s", "you"]);
        assert_eq!(terms(Stop, "It is THE end"), ["end"]);
        assert_eq!(terms(Whitespace, " Hello, World "), ["Hello,", "World"]);
        assert_eq!(terms(Keyword, "Hello World"), ["Hello World"]);
        assert!(terms(Keyword, "").is_empty());
    }

    #[test]
    fn porter_stems() {
        for (w, s) in [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("hopping", "hop"),
            ("tanned", "tan"),
            ("falling", "fall"),
            ("hissing", "hiss"),
            ("fizzed", "fizz"),
            ("failing", "fail"),
            ("filing", "file"),
            ("happy", "happi"),
            ("sky", "sky"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("rational", "ration"),
            ("generalization", "gener"),
            ("running", "run"),
            ("hopefulness", "hope"),
            ("goodness", "good"),
            ("adjustable", "adjust"),
            ("replacement", "replac"),
            ("effective", "effect"),
            ("cease", "ceas"),
            ("controll", "control"),
            ("roll", "roll"),
            ("adoption", "adopt"),
            ("is", "is"),
        ] {
            assert_eq!(porter(w), s, "{w}");
        }
    }

    #[test]
    fn query_syntax() {
        let t = |f: Option<&str>, s: &str| Query::Term { field: f.map(String::from), text: s.into() };
        assert_eq!(QueryParser::parse("a b").unwrap(), Query::Bool(vec![(Occur::Should, t(None, "a")), (Occur::Should, t(None, "b"))]));
        assert_eq!(
            QueryParser::parse("a AND b OR -c").unwrap(),
            Query::Bool(vec![(Occur::Must, t(None, "a")), (Occur::Must, t(None, "b")), (Occur::MustNot, t(None, "c"))])
        );
        assert_eq!(
            QueryParser::parse("title:(x +y) NOT z^2").unwrap(),
            Query::Bool(vec![
                (Occur::Should, Query::Bool(vec![(Occur::Should, t(Some("title"), "x")), (Occur::Must, t(Some("title"), "y"))])),
                (Occur::MustNot, Query::Boost(Box::new(t(None, "z")), 2.0)),
            ])
        );
        assert_eq!(
            QueryParser::parse(r#"t:"a b" c* d~1 e\:f ANDROID *:*"#).unwrap(),
            Query::Bool(vec![
                (Occur::Should, Query::Phrase { field: Some("t".into()), text: "a b".into() }),
                (Occur::Should, Query::Wildcard { field: None, pattern: "c*".into() }),
                (Occur::Should, Query::Fuzzy { field: None, term: "d".into(), edits: 1 }),
                (Occur::Should, t(None, "e:f")),
                (Occur::Should, t(None, "ANDROID")),
                (Occur::Should, Query::All),
            ])
        );
        for bad in ["", "  ", "AND a", "a AND", "(a", "\"a", "a)", "a^", "f:"] {
            assert!(QueryParser::parse(bad).is_err(), "{bad}");
        }
        for unsupported in ["[a TO b]", "/ab+/", "\"a b\"~2"] {
            assert!(matches!(QueryParser::parse(unsupported), Err(Error::Unsupported(_))), "{unsupported}");
        }
    }

    #[test]
    fn matching_helpers() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert!(wildcard_match(&c("ma*ix"), &c("matrix")));
        assert!(wildcard_match(&c("m?t*"), &c("mat")));
        assert!(!wildcard_match(&c("ma?"), &c("matrix")));
        assert_eq!(edit_distance(&c("matrix"), &c("matirx"), 2), Some(1));
        assert_eq!(edit_distance(&c("matrix"), &c("mattress"), 2), None);
        assert_eq!(edit_distance(&c("kitten"), &c("sitting"), 3), Some(3));
    }

    #[test]
    fn commits_update_the_index() {
        let db = crate::Database::in_memory();
        let q = "CALL db.index.fulltext.queryNodes('t', 'matrix') YIELD node RETURN node.k ORDER BY node.k";
        let keys = |db: &crate::Database<graphbuff_core::MemoryGraph>| -> Vec<String> {
            db.execute(q).unwrap().rows.iter().map(|r| r[0].to_string()).collect()
        };
        db.execute("CREATE FULLTEXT INDEX t FOR (n:M|S) ON EACH [n.title]").unwrap();
        db.execute("UNWIND range(1, 20) AS i CREATE (:M {k: i, title: 'Movie ' + i})").unwrap();
        db.execute("CREATE (:M {k: 100, title: 'The Matrix'}), (:S {k: 101, title: ['matrix', 'reloaded']})").unwrap();
        assert_eq!(keys(&db), ["100", "101"]);
        let built = db.text_cache.entry("t").expect("built");
        db.execute("MATCH (n:M {k: 3}) SET n.title = 'Matrix Revolutions'").unwrap();
        db.execute("MATCH (n {k: 100}) REMOVE n:M").unwrap();
        db.execute("MATCH (n {k: 101}) DETACH DELETE n").unwrap();
        assert_eq!(keys(&db), ["3"]);
        let patched = db.text_cache.entry("t").expect("kept");
        assert!(!std::sync::Arc::ptr_eq(&built, &patched));
        assert_eq!(patched.docs.len(), 20);
        // The postings of terms no entity has any more are freed.
        assert!(!patched.terms.contains_key("reloaded"));
        db.execute("DROP INDEX t").unwrap();
        assert!(db.text_cache.entry("t").is_none());
    }
}
