//! Finding a node by what people call it (spec 031): its id, the name chosen for its type, the
//! schema's name for it and its contact-like properties, matched whatever the case, the accents
//! and the order of the words, and ranked: exact, then prefix, then the start of a word, then
//! anywhere.
//!
//! Each type of at most [`INDEX_MAX_NODES`] nodes is matched in memory, against its name index:
//! every node's id and searched properties, read once at a commit (`builder::node_index`)
//! through the session's read memo, so a commit that changed no node of the type carries it
//! and one that did reads it again. Indexes are held by type and commit, at most
//! [`INDEX_BUDGET_BYTES`] of text in all, the least recently used let go first. A larger type,
//! or one whose index outgrew the limit, is searched on the server: each word in any of four
//! spellings (`builder::node_search`). Either way, a property with a full-text index is also
//! searched with the engine's `search`, ranked by `bm25`, and what it finds is merged in after
//! the matches above. A full-text read that fails is left out; a search the type cannot do
//! without (its index, or the server's search) fails the type.
//!
//! Rows are read as data, never logged: neither the text searched nor what it found.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::Shared;
use gpui::{AppContext as _, Context, SharedString, Task};
use gq_language::builder;
use gq_language::{NodeType, SchemaModel};
use serde_json::Value;
use unicode_normalization::UnicodeNormalization as _;
use unicode_normalization::char::is_combining_mark;

use crate::scope::ReadScope;
use crate::{EntityKind, GraphSession};

/// The most nodes of one type matched in memory; a larger type is searched on the server.
pub const INDEX_MAX_NODES: usize = 5_000;
/// The most the name indexes hold together, in bytes of ids and texts (each kept twice: as
/// read and folded) and a fixed cost a node; the least recently used index goes first beyond
/// it.
pub const INDEX_BUDGET_BYTES: usize = 32 * 1024 * 1024;
/// What an index counts a node as besides its texts: its entry and the strings' headers.
const ENTRY_BYTES: usize = 128;
/// The characters of each property's text an index keeps: a whole name or title, even a long
/// one, never a whole document.
pub const INDEX_TEXT_CHARS: usize = 1_000;
/// The rows a server search asks for, of which the best are shown.
pub const SERVER_ROWS: u32 = 25;
/// The full-text properties searched for a type held in memory (those its index does not
/// hold), and for one searched on the server.
pub const TEXT_SEARCHES_HELD: usize = 1;
pub const TEXT_SEARCHES_SERVER: usize = 2;
/// The shortest text sent to the server: one character matches nearly every node.
pub const SERVER_MIN_CHARS: usize = 2;

/// How well a node matched the text, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchRank {
    /// A text is the text typed, after folding.
    Exact,
    /// A text starts with it.
    Prefix,
    /// Every word typed starts a word of one text.
    WordStart,
    /// Every word typed is somewhere in the node's texts.
    Substring,
    /// Only the engine's full-text search found it (a stemmed word, a long text not held).
    FullText,
}

/// A node a search found.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeHit {
    pub id: String,
    /// The text of the property that names the type's nodes, when that is not the key.
    pub label: Option<String>,
    /// The property the text matched, with its text, when that is neither the id nor the name.
    pub matched: Option<(String, String)>,
    pub rank: MatchRank,
}

/// One type's answer: its best hits, best first, and whether more matched than are listed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TypeHits {
    pub hits: Vec<NodeHit>,
    pub more: bool,
    /// Whether the type was matched in memory rather than searched on the server.
    pub held: bool,
}

/// Text for matching: decomposed and stripped of accents, lowercase, apostrophes dropped
/// ("O'Brien" → "obrien") and every other run of non-letters one space ("jean-luc" → "jean
/// luc", "anna.mueller@db.com" → "anna mueller db com").
pub fn fold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    let mut after_space = true;
    for c in text.nfkd() {
        if is_combining_mark(c) || c == '\'' || c == '\u{2019}' {
            continue;
        }
        if c.is_alphanumeric() {
            folded.extend(c.to_lowercase());
            after_space = false;
        } else if !after_space {
            folded.push(' ');
            after_space = true;
        }
    }
    if folded.ends_with(' ') {
        folded.pop();
    }
    folded
}

/// The text typed, folded, and its words.
struct Wanted {
    folded: String,
    words: Vec<String>,
}

impl Wanted {
    fn new(text: &str) -> Self {
        let folded = fold(text);
        let words = folded
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect();
        Self { folded, words }
    }

    /// How well one folded text matches.
    fn rank(&self, text: &str) -> Option<MatchRank> {
        if text.is_empty() {
            return None;
        }
        if text == self.folded {
            Some(MatchRank::Exact)
        } else if text.starts_with(&self.folded) {
            Some(MatchRank::Prefix)
        } else if self.words.iter().all(|word| starts_a_word(text, word)) {
            Some(MatchRank::WordStart)
        } else if self.words.iter().all(|word| text.contains(word.as_str())) {
            Some(MatchRank::Substring)
        } else {
            None
        }
    }
}

fn starts_a_word(text: &str, word: &str) -> bool {
    text.match_indices(word)
        .any(|(at, _)| at == 0 || text.as_bytes()[at - 1] == b' ')
}

/// Whether `hit`, found for earlier text, also matches `text` as a search ranks it, by its id,
/// name and the text it matched: a list keeps such a row while the new text's answers are read.
/// A node only full-text search found is never kept: the engine decided it.
pub fn hit_matches(hit: &NodeHit, text: &str) -> bool {
    if text.trim().is_empty() {
        return true;
    }
    if hit.rank == MatchRank::FullText {
        return false;
    }
    let wanted = Wanted::new(text);
    if wanted.words.is_empty() {
        return false;
    }
    let texts: Vec<String> = std::iter::once(hit.id.as_str())
        .chain(hit.label.as_deref())
        .chain(hit.matched.as_ref().map(|(_, text)| text.as_str()))
        .map(fold)
        .collect();
    texts.iter().any(|text| wanted.rank(text).is_some())
        || wanted
            .words
            .iter()
            .all(|word| texts.iter().any(|text| text.contains(word.as_str())))
}

/// One node of an index: its id and its searched properties' texts, each with its folding.
#[derive(Debug, Clone)]
struct Entry {
    id: String,
    id_folded: String,
    texts: Vec<Option<(String, String)>>,
}

/// A type's nodes as a search matches them: from its name index, or from a server search's
/// rows. `properties` are `builder::searched_properties`, `label` the one of them that names
/// the type's nodes.
#[derive(Debug, Clone)]
pub(crate) struct NodeIndex {
    properties: Vec<String>,
    label: Option<usize>,
    entries: Vec<Entry>,
    /// What it holds, counted as [`INDEX_BUDGET_BYTES`] counts.
    bytes: usize,
}

impl NodeIndex {
    /// The index of `node`'s rows as `builder::node_index` and `builder::node_search` return
    /// them (`id`, `p0`, `p1`, …), each text cut to [`INDEX_TEXT_CHARS`].
    fn from_rows(node: &NodeType, rows: &Value) -> Self {
        let properties: Vec<String> = builder::searched_properties(node)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let label = node
            .label_property()
            .and_then(|label| properties.iter().position(|p| p == label));
        let entries = rows
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let id = row.get("id")?.as_str()?.to_owned();
                let texts = (0..properties.len())
                    .map(|at| {
                        let text = row.get(builder::searched_column(at))?.as_str()?;
                        let text: String = text.chars().take(INDEX_TEXT_CHARS).collect();
                        let folded = fold(&text);
                        Some((text, folded))
                    })
                    .collect();
                Some(Entry {
                    id_folded: fold(&id),
                    id,
                    texts,
                })
            })
            .collect::<Vec<Entry>>();
        let bytes = entries
            .iter()
            .map(|entry| {
                ENTRY_BYTES
                    + entry.id.len()
                    + entry.id_folded.len()
                    + entry
                        .texts
                        .iter()
                        .flatten()
                        .map(|(text, folded)| text.len() + folded.len())
                        .sum::<usize>()
            })
            .sum();
        Self {
            properties,
            label,
            entries,
            bytes,
        }
    }

    /// Whether it holds what a search of `node` reads now: the searched properties change
    /// when another property is chosen to name the type.
    fn reads(&self, node: &NodeType) -> bool {
        builder::searched_properties(node)
            .into_iter()
            .eq(self.properties.iter().map(String::as_str))
    }

    fn hit(&self, entry: &Entry, rank: MatchRank, matched_at: Option<usize>) -> NodeHit {
        let label = self
            .label
            .and_then(|at| entry.texts[at].as_ref())
            .map(|(text, _)| text.clone());
        let matched = matched_at
            .filter(|at| Some(*at) != self.label)
            .and_then(|at| {
                let (text, _) = entry.texts[at].as_ref()?;
                Some((self.properties[at].clone(), text.clone()))
            });
        NodeHit {
            id: entry.id.clone(),
            label,
            matched,
            rank,
        }
    }

    /// The best `limit` nodes for `text`, ranked; with no text, the first `limit` by id.
    fn find(&self, text: &str, limit: usize) -> TypeHits {
        let wanted = Wanted::new(text);
        if text.trim().is_empty() {
            return TypeHits {
                hits: self
                    .entries
                    .iter()
                    .take(limit)
                    .map(|entry| self.hit(entry, MatchRank::Substring, None))
                    .collect(),
                more: self.entries.len() > limit,
                held: true,
            };
        }
        if wanted.words.is_empty() {
            return TypeHits {
                held: true,
                ..TypeHits::default()
            };
        }
        // (rank, matched on a property other than the name or id, the text's length, place)
        let mut found: Vec<(MatchRank, bool, usize, usize, Option<usize>)> = Vec::new();
        for (place, entry) in self.entries.iter().enumerate() {
            let mut best: Option<(MatchRank, bool, usize, Option<usize>)> = wanted
                .rank(&entry.id_folded)
                .map(|rank| (rank, false, entry.id_folded.len(), None));
            for (at, text) in entry.texts.iter().enumerate() {
                let Some((_, folded)) = text else { continue };
                if let Some(rank) = wanted.rank(folded) {
                    let other = Some(at) != self.label;
                    let candidate = (rank, other, folded.len(), Some(at));
                    if best.is_none_or(|best| {
                        (candidate.0, candidate.1, candidate.2) < (best.0, best.1, best.2)
                    }) {
                        best = Some(candidate);
                    }
                }
            }
            // Words found across the node's texts, none holding them all ("acme renewal": a
            // name and a note).
            if best.is_none() {
                let across = wanted.words.iter().all(|word| {
                    entry.id_folded.contains(word.as_str())
                        || entry
                            .texts
                            .iter()
                            .flatten()
                            .any(|(_, folded)| folded.contains(word.as_str()))
                });
                if across {
                    best = Some((MatchRank::Substring, true, usize::MAX, self.label));
                }
            }
            if let Some((rank, other, length, at)) = best {
                found.push((rank, other, length, place, at));
            }
        }
        let more = found.len() > limit;
        if found.len() > limit {
            found.select_nth_unstable(limit);
            found.truncate(limit);
        }
        found.sort();
        TypeHits {
            hits: found
                .into_iter()
                .map(|(rank, _, _, place, at)| self.hit(&self.entries[place], rank, at))
                .collect(),
            more,
            held: true,
        }
    }
}

/// Adds what the engine's full-text search found (`builder::node_text_search` answers, each
/// most relevant first) after `hits`, each node once, up to `limit`.
fn merge_full_text(
    node: &NodeType,
    hits: &mut TypeHits,
    answers: &[(String, Arc<Value>)],
    limit: usize,
) {
    let mut seen: HashSet<String> = hits.hits.iter().map(|hit| hit.id.clone()).collect();
    for (property, rows) in answers {
        let rows = rows.as_array().map(Vec::as_slice).unwrap_or_default();
        let found = NodeIndex::from_rows(node, &Value::Array(rows.to_vec()));
        for (entry, row) in found.entries.iter().zip(rows) {
            if !seen.insert(entry.id.clone()) {
                continue;
            }
            if hits.hits.len() >= limit {
                hits.more = true;
                return;
            }
            let mut hit = found.hit(entry, MatchRank::FullText, None);
            let text = row
                .get("matched")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if Some(property.as_str()) != node.label_property() {
                hit.matched = Some((
                    property.clone(),
                    text.chars().take(INDEX_TEXT_CHARS).collect(),
                ));
            }
            hits.hits.push(hit);
        }
    }
}

/// What reading a type's index came to.
#[derive(Clone)]
enum Loaded {
    Index(Arc<NodeIndex>),
    /// More nodes than [`INDEX_MAX_NODES`]: searched on the server.
    TooLarge,
}

type Loading = Shared<Task<Result<(Option<String>, Loaded), SharedString>>>;

/// The name indexes a session holds, by type and commit, within [`INDEX_BUDGET_BYTES`].
#[derive(Default)]
pub(crate) struct NodeIndexes {
    held: HashMap<(String, String), (Loaded, u64)>,
    /// Indexes being read, by type and the commit asked for (`None`: the branch's head).
    loading: HashMap<(String, Option<String>), Loading>,
    used: u64,
    /// Moves when the indexes are forgotten (a new token, a new schema), so a read still out
    /// from before lands nowhere.
    generation: u64,
}

impl NodeIndexes {
    /// Forgets every index, and every read still out: their answers land nowhere.
    pub(crate) fn reset(&mut self) {
        self.held.clear();
        self.loading.clear();
        self.generation += 1;
    }

    fn get(&mut self, type_name: &str, commit: &str, node: &NodeType) -> Option<Loaded> {
        self.used += 1;
        let used = self.used;
        let (loaded, at) = self
            .held
            .get_mut(&(type_name.to_owned(), commit.to_owned()))?;
        if let Loaded::Index(index) = loaded
            && !index.reads(node)
        {
            return None;
        }
        *at = used;
        Some(loaded.clone())
    }

    fn insert(&mut self, type_name: &str, commit: String, loaded: Loaded) {
        self.used += 1;
        let key = (type_name.to_owned(), commit);
        self.held.insert(key.clone(), (loaded, self.used));
        let size = |loaded: &Loaded| match loaded {
            Loaded::Index(index) => index.bytes,
            Loaded::TooLarge => 0,
        };
        let mut total: usize = self.held.values().map(|(loaded, _)| size(loaded)).sum();
        while total > INDEX_BUDGET_BYTES {
            let Some(oldest) = self
                .held
                .iter()
                .filter(|(held, _)| **held != key)
                .min_by_key(|(_, (_, used))| *used)
                .map(|(held, _)| held.clone())
            else {
                break;
            };
            if let Some((loaded, _)) = self.held.remove(&oldest) {
                total -= size(&loaded);
            }
        }
    }

    /// What the indexes hold in all, as [`INDEX_BUDGET_BYTES`] counts it.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.held
            .values()
            .map(|(loaded, _)| match loaded {
                Loaded::Index(index) => index.bytes,
                Loaded::TooLarge => 0,
            })
            .sum()
    }
}

/// Where a search's reads go: the commit (the branch's head when `None`), and the scope they
/// are read through, when the caller holds one.
#[derive(Clone)]
struct ReadAt {
    commit: Option<String>,
    scope: Option<ReadScope>,
}

fn message(error: impl std::fmt::Display) -> SharedString {
    error.to_string().into()
}

impl GraphSession {
    /// The nodes of `type_name` that `text` names, best first, at most `limit` (spec 031): from
    /// the type's name index when it holds at most [`INDEX_MAX_NODES`] nodes, else from the
    /// server; with what full-text search finds merged in. Read at `snapshot`, or the branch's
    /// head. Fails only when the type cannot be searched at all: its index or the server's
    /// search refused or unanswered.
    ///
    /// Read through `scope` when one is given (a [`crate::ledger::ReadView::Search`] scope the
    /// caller holds while it searches): the change feed is then asked about the types searched,
    /// so a commit that changed none of a type's nodes carries its index.
    pub fn find_nodes(
        &mut self,
        type_name: &str,
        text: &str,
        snapshot: Option<String>,
        scope: Option<&ReadScope>,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<TypeHits, SharedString>> {
        let Some(schema) = self.schema_model() else {
            return Task::ready(Err("The graph's schema is not read yet.".into()));
        };
        let Some(node) = schema.node(type_name).cloned() else {
            return Task::ready(Err(
                format!("The graph has no node type {type_name}.").into()
            ));
        };
        let text = text.trim().to_owned();
        let at = ReadAt {
            commit: snapshot.or_else(|| self.head(&self.branch).map(str::to_owned)),
            scope: scope.cloned(),
        };
        // The type's count, at the branch's newest commit, says whether to hold it; an index read
        // that finds more nodes than the limit says so too.
        let counted_large = self
            .count_of(EntityKind::Node, type_name)
            .is_some_and(|count| count > INDEX_MAX_NODES as i64);
        if counted_large {
            return self.server_find(&schema, node, text, at, limit, cx);
        }
        let index = self.node_index(&schema, &node, &at, cx);
        // What the index does not hold, searched by the engine meanwhile.
        let held: Vec<&str> = builder::searched_properties(&node);
        let properties: Vec<String> = node
            .properties
            .iter()
            .filter(|p| p.full_text && !p.is_key && !held.contains(&p.name.as_str()))
            .take(TEXT_SEARCHES_HELD)
            .map(|p| p.name.clone())
            .collect();
        let texts = self.full_text_reads(&schema, &node, &properties, &text, &at, cx);
        let schema_held = schema.clone();
        cx.spawn(async move |this, cx| {
            let (commit, loaded) = index.await?;
            match loaded {
                Loaded::Index(index) => {
                    let find_text = text.clone();
                    let mut hits = cx
                        .background_spawn(async move { index.find(&find_text, limit) })
                        .await;
                    let answers = full_text_answers(texts).await;
                    merge_full_text(&node, &mut hits, &answers, limit);
                    Ok(hits)
                }
                Loaded::TooLarge => {
                    drop(texts);
                    let find = this
                        .update(cx, |this, cx| {
                            let at = ReadAt { commit, ..at };
                            this.server_find(&schema_held, node, text, at, limit, cx)
                        })
                        .map_err(|_| SharedString::from("The graph was closed."))?;
                    find.await
                }
            }
        })
    }

    /// `node`'s name index at `commit` (the branch's head when `None`), held or read, and the
    /// commit it is of.
    fn node_index(
        &mut self,
        schema: &Arc<SchemaModel>,
        node: &NodeType,
        at: &ReadAt,
        cx: &mut Context<Self>,
    ) -> Task<Result<(Option<String>, Loaded), SharedString>> {
        let commit = at.commit.clone();
        if let Some(commit) = &commit
            && let Some(loaded) = self.node_indexes.get(&node.name, commit, node)
        {
            return Task::ready(Ok((Some(commit.clone()), loaded)));
        }
        let key = (node.name.clone(), commit.clone());
        if let Some(loading) = self.node_indexes.loading.get(&key) {
            let loading = loading.clone();
            return cx.background_spawn(loading);
        }
        let query = match builder::node_index(schema, &node.name, INDEX_MAX_NODES as u32 + 1) {
            Ok(query) => query,
            Err(error) => return Task::ready(Err(message(error))),
        };
        let read = self.read_through(at, &query, cx);
        let (node, type_name) = (node.clone(), node.name.clone());
        let generation = self.node_indexes.generation;
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            let finished = match result {
                Ok(output) => {
                    let at = output.graph_commit_id.clone().or(commit.clone());
                    let rows = output.rows.clone();
                    let loaded = cx
                        .background_spawn(async move {
                            match rows.as_array().map_or(0, Vec::len) > INDEX_MAX_NODES {
                                true => Loaded::TooLarge,
                                false => {
                                    Loaded::Index(Arc::new(NodeIndex::from_rows(&node, &rows)))
                                }
                            }
                        })
                        .await;
                    Ok((at, loaded))
                }
                Err(error) => Err(message(error)),
            };
            this.update(cx, |this, _| {
                // Forgotten while it was out (a new token, a new schema): it lands nowhere.
                if this.node_indexes.generation != generation {
                    return;
                }
                this.node_indexes
                    .loading
                    .remove(&(type_name.clone(), commit));
                if let Ok((Some(at), loaded)) = &finished {
                    this.node_indexes
                        .insert(&type_name, at.clone(), loaded.clone());
                }
            })
            .ok();
            finished
        });
        let shared = task.shared();
        self.node_indexes.loading.insert(key, shared.clone());
        cx.background_spawn(shared)
    }

    /// [`GraphSession::read_in`] `scope` when there is one, else [`GraphSession::read`].
    fn read_through(
        &mut self,
        at: &ReadAt,
        query: &builder::BuiltQuery,
        cx: &mut Context<Self>,
    ) -> Task<Result<graph_client::api::ReadOutput, graph_client::ReadError>> {
        match &at.scope {
            Some(scope) => self.read_in(scope, query, at.commit.clone(), cx),
            None => self.read(query, at.commit.clone(), cx),
        }
    }

    /// The engine's full-text search of each of `properties` for `text`, at `commit`.
    fn full_text_reads(
        &mut self,
        schema: &SchemaModel,
        node: &NodeType,
        properties: &[String],
        text: &str,
        at: &ReadAt,
        cx: &mut Context<Self>,
    ) -> Vec<(
        String,
        Task<Result<graph_client::api::ReadOutput, graph_client::ReadError>>,
    )> {
        if text.chars().count() < SERVER_MIN_CHARS {
            return Vec::new();
        }
        properties
            .iter()
            .filter_map(|property| {
                let query =
                    builder::node_text_search(schema, &node.name, property, text, SERVER_ROWS)
                        .ok()?;
                Some((property.clone(), self.read_through(at, &query, cx)))
            })
            .collect()
    }

    /// The server's search of a type too large to hold (`builder::node_search`), ranked as an
    /// index ranks, with full-text search of its indexed name and contact properties, else its
    /// key.
    fn server_find(
        &mut self,
        schema: &Arc<SchemaModel>,
        node: NodeType,
        text: String,
        at: ReadAt,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<TypeHits, SharedString>> {
        let query = match builder::node_search(schema, &node.name, &text, SERVER_ROWS) {
            Ok(query) => query,
            Err(error) => return Task::ready(Err(message(error))),
        };
        let typed = text.chars().count() >= SERVER_MIN_CHARS || text.is_empty();
        let read = typed.then(|| self.read_through(&at, &query, cx));
        // Words on their own match many more nodes than the phrase they make: a name typed in
        // full is asked for whole too, so it is among the rows read whatever its id.
        let phrase = (typed && text.split_whitespace().nth(1).is_some())
            .then(|| builder::node_phrase_search(schema, &node.name, &text, SERVER_ROWS).ok())
            .flatten()
            .map(|query| self.read_through(&at, &query, cx));
        let searched = builder::searched_properties(&node);
        let mut properties: Vec<String> = node
            .properties
            .iter()
            .filter(|p| p.full_text && searched.contains(&p.name.as_str()))
            .map(|p| p.name.clone())
            .collect();
        properties.extend(
            node.key
                .clone()
                .filter(|key| node.property(key).is_some_and(|p| p.full_text)),
        );
        properties.truncate(TEXT_SEARCHES_SERVER);
        let texts = self.full_text_reads(schema, &node, &properties, &text, &at, cx);
        cx.spawn(async move |_, cx| {
            let Some(read) = read else {
                return Ok(TypeHits::default());
            };
            let output = read.await.map_err(message)?;
            // The phrase's rows first: the words' rows hold them too, unless their limit cut
            // them off.
            let mut rows: Vec<Value> = match phrase {
                Some(phrase) => phrase
                    .await
                    .ok()
                    .and_then(|output| output.rows.as_array().cloned())
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            let asked = output.rows.as_array().map_or(0, Vec::len);
            for row in output.rows.as_array().into_iter().flatten() {
                if !rows.iter().any(|held| held.get("id") == row.get("id")) {
                    rows.push(row.clone());
                }
            }
            let rows = Value::Array(rows);
            let answers = full_text_answers(texts).await;
            let hits = cx
                .background_spawn(async move {
                    let index = NodeIndex::from_rows(&node, &rows);
                    let mut hits = index.find(&text, limit);
                    // Every row the server matched is listed, after the ranked ones, even one
                    // folding would not place (a text longer than an index keeps).
                    for entry in &index.entries {
                        if hits.hits.iter().any(|hit| hit.id == entry.id) {
                            continue;
                        }
                        if hits.hits.len() >= limit {
                            hits.more = true;
                            break;
                        }
                        hits.hits.push(index.hit(entry, MatchRank::Substring, None));
                    }
                    hits.more |= asked >= SERVER_ROWS as usize;
                    hits.held = false;
                    merge_full_text(&node, &mut hits, &answers, limit);
                    hits
                })
                .await;
            Ok(hits)
        })
    }
}

/// What each full-text read found, by property; a read that failed is left out (the property
/// has no index yet, the engine refused it), since the type's own search still answers.
async fn full_text_answers(
    reads: Vec<(
        String,
        Task<Result<graph_client::api::ReadOutput, graph_client::ReadError>>,
    )>,
) -> Vec<(String, Arc<Value>)> {
    let mut answers = Vec::new();
    for (property, read) in reads {
        if let Ok(output) = read.await {
            answers.push((property, output.rows));
        }
    }
    answers
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn folding_drops_case_accents_and_punctuation() {
        assert_eq!(fold("Société Générale"), "societe generale");
        assert_eq!(fold("Seán O'Brien"), "sean obrien");
        assert_eq!(fold("JPMorgan Chase & Co."), "jpmorgan chase co");
        assert_eq!(fold("anna.mueller@db.com"), "anna mueller db com");
        assert_eq!(fold("  Zürich  "), "zurich");
        assert_eq!(fold("McDonald's"), "mcdonalds");
    }

    fn org() -> NodeType {
        let schema = SchemaModel::parse(
            "node Org {\n  slug: String @key\n  name: String\n  website: String?\n}\n",
        )
        .unwrap();
        schema.node("Org").unwrap().clone()
    }

    fn index(rows: Value) -> NodeIndex {
        NodeIndex::from_rows(&org(), &rows)
    }

    fn ids(hits: &TypeHits) -> Vec<&str> {
        hits.hits.iter().map(|hit| hit.id.as_str()).collect()
    }

    fn acme() -> NodeIndex {
        let mut rows = vec![json!({"id": "acme.com", "p0": "ACME Corp"})];
        for name in ["Analytics", "Bank", "Cloud", "Health", "Labs", "Robotics"] {
            rows.push(json!({"id": format!("acme-{}.io", name.to_lowercase()), "p0": format!("Acme {name}")}));
        }
        rows.push(json!({"id": "openai.com", "p0": "OpenAI", "p1": "https://openai.com"}));
        rows.push(json!({"id": "mckinsey.com", "p0": "McKinsey & Company"}));
        rows.push(json!({"id": "db.com", "p0": "Deutsche Bank AG"}));
        index(Value::Array(rows))
    }

    /// Case, accents and word order do not matter; an exact name ranks before every node that
    /// only contains it, whatever their ids (spec 031).
    #[test]
    fn matches_are_found_whatever_the_case_and_ranked_exact_first() {
        let index = acme();
        assert_eq!(ids(&index.find("openai", 5)), ["openai.com"]);
        assert_eq!(index.find("openai", 5).hits[0].rank, MatchRank::Exact);
        assert_eq!(ids(&index.find("mckinsey", 5)), ["mckinsey.com"]);
        assert_eq!(ids(&index.find("bank deutsche", 5)), ["db.com"]);
        assert_eq!(
            index.find("bank deutsche", 5).hits[0].rank,
            MatchRank::WordStart
        );
        let acme = index.find("acme corp", 5);
        assert_eq!(ids(&acme), ["acme.com"]);
        // "acme" names seven organisations; the one called ACME Corp sorts after six ids but
        // is a prefix match like the rest, and the shortest name comes first.
        let all = index.find("ACME", 5);
        assert_eq!(all.hits.len(), 5);
        assert!(all.more);
        assert_eq!(all.hits[0].id, "acme.com", "{:?}", ids(&all));
        assert_eq!(all.hits[0].label.as_deref(), Some("ACME Corp"));
        // Inside a word, last.
        assert_eq!(ids(&index.find("insey", 5)), ["mckinsey.com"]);
        assert_eq!(index.find("insey", 5).hits[0].rank, MatchRank::Substring);
    }

    /// A match on a property other than the name says which, with its text.
    #[test]
    fn a_match_off_the_name_names_its_property() {
        let found = acme().find("https://openai", 5);
        assert_eq!(found.hits[0].id, "openai.com");
        assert_eq!(
            found.hits[0].matched,
            Some(("website".into(), "https://openai.com".into()))
        );
        assert_eq!(found.hits[0].label.as_deref(), Some("OpenAI"));
        assert_eq!(acme().find("openai", 5).hits[0].matched, None);
    }

    #[test]
    fn a_hit_kept_for_new_text_still_matches_it() {
        let hit = acme().find("open", 5).hits.remove(0);
        assert!(hit_matches(&hit, "openai"));
        assert!(hit_matches(&hit, "OPENAI.COM"));
        assert!(!hit_matches(&hit, "openaix"));
        let full_text = NodeHit {
            rank: MatchRank::FullText,
            ..hit
        };
        assert!(!hit_matches(&full_text, "openai"));
    }

    #[test]
    fn no_text_lists_the_first_nodes_and_punctuation_alone_finds_nothing() {
        let index = acme();
        assert_eq!(index.find("", 2).hits.len(), 2);
        assert!(index.find("", 2).more);
        assert!(index.find("&", 5).hits.is_empty());
    }

    /// Full-text hits come after the folded matches, each node once, in the engine's order.
    #[test]
    fn full_text_hits_follow_the_matches_once_each() {
        let node = org();
        let mut hits = acme().find("openai", 5);
        let answers = vec![(
            "slug".to_owned(),
            Arc::new(json!([
                {"id": "openai.com", "p0": "OpenAI", "matched": "openai.com"},
                {"id": "db.com", "p0": "Deutsche Bank AG", "matched": "db.com"},
            ])),
        )];
        merge_full_text(&node, &mut hits, &answers, 5);
        assert_eq!(ids(&hits), ["openai.com", "db.com"]);
        assert_eq!(hits.hits[1].rank, MatchRank::FullText);
        assert_eq!(hits.hits[1].label.as_deref(), Some("Deutsche Bank AG"));
    }

    /// The indexes hold at most the budget: the least recently used type goes first.
    #[test]
    fn indexes_are_held_within_their_budget() {
        let node = org();
        // Each node a long name: a third of the budget is a few thousand of them.
        let name = "n".repeat(INDEX_TEXT_CHARS);
        let big = |n: usize| {
            let rows: Vec<Value> = (0..n)
                .map(|i| json!({"id": format!("o{i:06}"), "p0": name}))
                .collect();
            Loaded::Index(Arc::new(NodeIndex::from_rows(&node, &Value::Array(rows))))
        };
        let per_node = match big(1) {
            Loaded::Index(index) => index.bytes,
            Loaded::TooLarge => unreachable!(),
        };
        let share = INDEX_BUDGET_BYTES / 3 / per_node + 1;
        let mut indexes = NodeIndexes::default();
        indexes.insert("A", "c1".into(), big(share));
        indexes.insert("B", "c1".into(), big(share));
        assert!(indexes.get("A", "c1", &node).is_some(), "A used after B");
        indexes.insert("C", "c1".into(), big(share));
        assert!(indexes.bytes() <= INDEX_BUDGET_BYTES);
        assert!(indexes.get("A", "c1", &node).is_some());
        assert!(
            indexes.get("B", "c1", &node).is_none(),
            "the least recently used went"
        );
        assert!(indexes.get("C", "c1", &node).is_some());
    }

    /// How many reads of each name a fake server answered.
    type Reads = Arc<std::sync::Mutex<HashMap<String, usize>>>;
    type Served = Arc<std::sync::Mutex<crate::test_server::Served>>;

    /// The rows a fake server answers each node search with, and how many of each it read.
    fn served_people(
        people: usize,
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<GraphSession>, Served, Reads) {
        let reads: Reads = Arc::default();
        let counted = reads.clone();
        let (session, served) = crate::test_server::followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\n\
             node Company {\n  slug: String @key\n  name: String\n}\n",
            move |asked| {
                let name = asked["name"].as_str().unwrap_or_default().to_owned();
                *counted.lock().unwrap().entry(name.clone()).or_default() += 1;
                match name.as_str() {
                    "studio_search_index" => Value::Array(
                        (0..people)
                            .map(|i| json!({"id": format!("p{i:05}"), "p0": format!("Person {i}")}))
                            .chain([json!({"id": "zz-ada", "p0": "Ada Lovelace"})])
                            .collect(),
                    ),
                    "studio_find" => json!([{"id": "zz-ada", "p0": "Ada Lovelace"}]),
                    // The phrase finds the one person named it in full; each word on its own
                    // finds 25 others first by id.
                    "studio_find_phrase" => json!([{"id": "zz-lovelace", "p0": "Lovelace Ada"}]),
                    _ => json!([]),
                }
            },
            cx,
        );
        (session, served, reads)
    }

    fn find(
        session: &gpui::Entity<GraphSession>,
        scope: Option<&ReadScope>,
        text: &str,
        cx: &mut gpui::TestAppContext,
    ) -> TypeHits {
        let task = session.update(cx, |session, cx| {
            session.find_nodes("Person", text, None, scope, 5, cx)
        });
        cx.run_until_parked();
        futures::executor::block_on(task).expect("the search answers")
    }

    /// A type's index is read once at a commit and matched in memory for every text after;
    /// a commit that changed no node of the type carries it, one that changed one reads it
    /// again (spec 031).
    #[gpui::test]
    async fn an_index_is_read_once_a_commit_and_carried_past_other_types(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session, served, reads) = served_people(3, cx);
        let _watch = session.update(cx, |session, cx| session.watch_head("main", cx));
        let scope = session.update(cx, |session, _| {
            session.read_scope("main", crate::ledger::ReadView::Search)
        });
        let scope = Some(&scope);
        cx.run_until_parked();
        let indexes = |reads: &Reads| {
            reads
                .lock()
                .unwrap()
                .get("studio_search_index")
                .copied()
                .unwrap_or(0)
        };
        let found = find(&session, scope, "lovelace ADA", cx);
        assert_eq!(found.hits[0].id, "zz-ada");
        assert!(found.held);
        assert_eq!(find(&session, scope, "ada", cx).hits.len(), 1);
        assert_eq!(find(&session, scope, "person", cx).hits.len(), 3);
        assert_eq!(indexes(&reads), 1, "one read for every text");
        assert!(reads.lock().unwrap().get("studio_find").is_none());

        served.lock().unwrap().commit(
            "c2",
            vec![crate::test_server::node_change("Company", "acme")],
        );
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(
            session.read_with(cx, |s, _| s.head("main").map(str::to_owned)),
            Some("c2".into())
        );
        assert_eq!(find(&session, scope, "ada", cx).hits.len(), 1);
        assert_eq!(indexes(&reads), 1, "carried past a commit to another type");

        served.lock().unwrap().commit(
            "c3",
            vec![crate::test_server::node_change("Person", "p00001")],
        );
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(find(&session, scope, "ada", cx).hits.len(), 1);
        assert_eq!(indexes(&reads), 2, "read again once a person changed");
    }

    /// A type with more nodes than an index holds is searched on the server instead.
    #[gpui::test]
    async fn a_type_too_large_to_hold_is_searched_on_the_server(cx: &mut gpui::TestAppContext) {
        let (session, _served, reads) = served_people(INDEX_MAX_NODES, cx);
        cx.run_until_parked();
        let found = find(&session, None, "ada", cx);
        assert_eq!(found.hits[0].id, "zz-ada");
        assert!(!found.held);
        assert_eq!(reads.lock().unwrap().get("studio_find").copied(), Some(1));
        assert_eq!(
            session.read_with(cx, |session, _| session.node_indexes.bytes()),
            0,
            "nothing held of it"
        );
    }

    /// A name typed in full on a type searched on the server is asked for whole too: each word
    /// on its own matches many nodes, and the server's 25 by id can leave it out.
    #[gpui::test]
    async fn a_phrase_is_asked_for_whole_on_the_server(cx: &mut gpui::TestAppContext) {
        let (session, _served, reads) = served_people(INDEX_MAX_NODES, cx);
        cx.run_until_parked();
        let found = find(&session, None, "lovelace ada", cx);
        assert_eq!(found.hits[0].id, "zz-lovelace", "{:?}", found.hits);
        assert_eq!(found.hits[0].rank, MatchRank::Exact);
        assert_eq!(
            reads.lock().unwrap().get("studio_find_phrase").copied(),
            Some(1)
        );
        let _ = find(&session, None, "ada", cx);
        assert_eq!(
            reads.lock().unwrap().get("studio_find_phrase").copied(),
            Some(1),
            "one word asks no phrase"
        );
    }

    /// A new token forgets the indexes and every read still out. The old token's index read
    /// answers nothing (the session refuses an answer read under another token), and its end
    /// leaves the new token's read of the same index alone: a pause meanwhile joins it rather
    /// than sending a third (review of #124).
    #[gpui::test]
    async fn an_index_read_out_when_the_token_changes_lands_nowhere(cx: &mut gpui::TestAppContext) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        let (session, served, _reads) = served_people(3, cx);
        cx.run_until_parked();
        // Index reads sent, counted as the server receives them.
        let sent = Arc::new(AtomicUsize::new(0));
        let wait = |served: &Served, secs: u64| {
            let sent = sent.clone();
            served.lock().unwrap().wait =
                Box::new(move |asked| match asked["name"] == "studio_search_index" {
                    true => {
                        sent.fetch_add(1, Ordering::SeqCst);
                        Duration::from_secs(secs)
                    }
                    false => Duration::ZERO,
                });
        };
        let search = |cx: &mut gpui::TestAppContext| {
            session.update(cx, |session, cx| {
                session.find_nodes("Person", "ada", None, None, 5, cx)
            })
        };
        wait(&served, 10);
        let old = search(cx);
        cx.run_until_parked();
        let connection = session.read_with(cx, |session, _| session.connection.clone());
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("another".into()), cx)
        });
        cx.run_until_parked();
        wait(&served, 20);
        let new = search(cx);
        cx.run_until_parked();
        assert_eq!(
            sent.load(Ordering::SeqCst),
            2,
            "the new token reads it again"
        );

        // The old token's read ends: it answers nothing, and is held nowhere.
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        assert!(futures::executor::block_on(old).is_err());
        assert_eq!(session.read_with(cx, |s, _| s.node_indexes.bytes()), 0);
        assert_eq!(
            session.read_with(cx, |s, _| s.node_indexes.loading.len()),
            1,
            "the new token's read is still known to be out"
        );
        let joined = search(cx);
        cx.run_until_parked();
        assert_eq!(
            sent.load(Ordering::SeqCst),
            2,
            "joined the new token's read"
        );

        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        for found in [new, joined] {
            assert_eq!(
                futures::executor::block_on(found).unwrap().hits[0].id,
                "zz-ada"
            );
        }
        assert_eq!(sent.load(Ordering::SeqCst), 2);
    }
}
