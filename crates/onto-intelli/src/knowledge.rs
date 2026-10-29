//! Knowledge: documents split into passages, found by keywords (BM25) and by
//! meaning (vectors), with a citation for each passage. Documents are not
//! entities: they never enter the ontology's graph, search or reasoning.
//!
//! * A document arrives as text (plain or Markdown: `#` headings open
//!   sections, `|` tables become tables, a form feed starts a page), as
//!   parsed blocks (headings, text, tables, pages: what a parser returns),
//!   or already split into passages, which keeps their ids and, with
//!   `embedding_model`, their embeddings (an import from another knowledge
//!   base copies its vectors when the model matches).
//! * Passages are cut at headings and at about [`PASSAGE_TARGET_CHARS`]
//!   (never more than [`PASSAGE_MAX_CHARS`]); a table becomes `label: value`
//!   lines, one data row each, readable without its header. These are
//!   nexc-engine's rules, so the same document gives the same passages there
//!   and here (docs/KNOWLEDGE.md).
//! * A search fuses two ranks by reciprocal rank fusion: BM25 over the
//!   passage and its section path, and similarity, which is the cosine of
//!   the caller's query embedding with passages embedded by the same model
//!   when the caller sends one, else the hashed word and trigram vectors of
//!   [`onto_core::similarity`]. Scores are 0 to 1 and compare within one search.
//! * Who reads a document: principals with a read role in its workspace, up
//!   to its sensitivity (`public` < `internal` < `confidential` <
//!   `restricted`). A search may lower the ceiling (`max_sensitivity`), and
//!   agents never get above `[knowledge] agent_max_sensitivity` (default
//!   `internal`). A document with an `acl` is read only by the principals
//!   (or entities) it lists, and by admins.
//!
//! Kept in the KV namespace `knowledge` (`d/<document>` and
//! `p/<document>/<ordinal>`), indexed in memory, and kept in step across
//! processes by store notices, like memory.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use base64::Engine as _;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use onto_core::similarity::{VectorIndex, cosine};
use onto_core::store::{Store, StoreNotice};
use onto_core::text::TextIndex;
use onto_core::time::{new_id, now_ms};
use onto_core::{Access, Error, Principal, Result};

use crate::config::KnowledgeConfig;

/// The KV namespace (a [`onto_core::store::REPLICATED_KV`] one).
pub const NS: &str = "knowledge";
/// Characters a passage aims for (about 400 tokens) and may not exceed.
pub const PASSAGE_TARGET_CHARS: usize = 1_600;
pub const PASSAGE_MAX_CHARS: usize = 2_400;
/// Passages kept per document; a longer one is cut there and says so.
pub const PASSAGES_MAX: usize = 20_000;
/// Reciprocal rank fusion constant.
const RRF_K: f64 = 60.0;
/// Candidates each ranking contributes before fusion.
const CANDIDATES: usize = 200;
/// The least hashed-vector similarity that counts as a match.
const MIN_HASHED_SIMILARITY: f32 = 0.25;
/// The least cosine of embeddings that counts as a match.
const MIN_EMBEDDING_SIMILARITY: f32 = 0.2;

/// How sensitive a document is: who may receive its passages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    #[default]
    Internal,
    Confidential,
    Restricted,
}

/// What a passage was made from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassageKind {
    #[default]
    Text,
    Table,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub workspace: String,
    pub name: String,
    /// A grouping of documents (a topic, a folder, a feed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    #[serde(default)]
    pub sensitivity: Sensitivity,
    /// Only these principals or entities (and admins) read it; empty: everyone allowed above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acl: Vec<String>,
    /// Of the text or the passages as they came in: the same content is kept once per workspace.
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Where it came from (a URL, a path, another system's id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Pages, slides or sheets, when the format has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    pub passages: usize,
    /// Cut at [`PASSAGES_MAX`] passages.
    #[serde(default)]
    pub truncated: bool,
    /// The model that embedded its passages, when they came with embeddings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Passage {
    pub id: String,
    pub document: String,
    pub ordinal: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    /// The headings above it: "Report › Revenue › By region".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub section_path: String,
    #[serde(default)]
    pub kind: PassageKind,
    pub content: String,
    /// `<model>:<base64 of little-endian f32s>`; never returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embedding: Option<String>,
}

impl Passage {
    fn embedding(&self) -> Option<(&str, Vec<f32>)> {
        let (model, b64) = self.embedding.as_deref()?.rsplit_once(':')?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        let v: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        Some((model, v))
    }

    fn set_embedding(&mut self, model: &str, v: &[f32]) {
        let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.embedding = Some(format!("{model}:{}", base64::engine::general_purpose::STANDARD.encode(bytes)));
    }

    /// What the hashed vectors embed: the passage with where it sits, so
    /// that it means something on its own.
    fn embed_text(&self, document: &str) -> String {
        if self.section_path.is_empty() {
            format!("{document}\n{}", self.content)
        } else {
            format!("{document} › {}\n{}", self.section_path, self.content)
        }
    }
}

/// One block of a parsed document.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Block {
    pub kind: BlockKind,
    #[serde(default)]
    pub level: Option<u8>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub page: Option<u32>,
    /// Rows of cells: `[["a", "b"]]`, or `[{"cells": ["a", "b"]}]` as gRPC sends them.
    #[serde(default, deserialize_with = "rows")]
    pub rows: Option<Vec<Vec<String>>>,
    #[serde(default)]
    pub header_rows: Option<usize>,
}

fn rows<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Vec<Vec<String>>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Row {
        Cells(Vec<String>),
        Message { cells: Vec<String> },
    }
    let rows: Option<Vec<Row>> = Option::deserialize(d)?;
    Ok(rows.map(|r| {
        r.into_iter()
            .map(|r| match r {
                Row::Cells(c) | Row::Message { cells: c } => c,
            })
            .collect()
    }))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Heading,
    #[default]
    Text,
    Table,
}

/// A passage that comes already cut.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PassageInput {
    #[serde(default)]
    pub id: Option<String>,
    pub content: String,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub section_path: Option<String>,
    #[serde(default)]
    pub kind: Option<PassageKind>,
    /// In the space of the input's `embedding_model`.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
}

/// `knowledge.ingest`: one of `text`, `blocks` or `passages`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct IngestInput {
    /// Keep this id; an existing document with it is replaced.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub sensitivity: Option<Sensitivity>,
    #[serde(default)]
    pub acl: Vec<String>,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub meta: Value,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blocks: Option<Vec<Block>>,
    /// Pages of `blocks` (when the parser counted them).
    #[serde(default)]
    pub pages: Option<u32>,
    #[serde(default)]
    pub passages: Option<Vec<PassageInput>>,
    /// The model of `passages[].embedding`.
    #[serde(default)]
    pub embedding_model: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IngestResult {
    pub document: Document,
    /// False when the same content was already there (nothing was written).
    pub created: bool,
    /// True when a document with the same id was replaced.
    pub replaced: bool,
}

/// `knowledge.update`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct UpdateInput {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// `""` removes it from its collection (as `clear_collection` does).
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub clear_collection: bool,
    #[serde(default)]
    pub sensitivity: Option<Sensitivity>,
    #[serde(default)]
    pub acl: Option<Vec<String>>,
    /// `acl` replaces the document's even when absent or empty (it clears it).
    #[serde(default)]
    pub set_acl: bool,
    #[serde(default)]
    pub meta: Option<Value>,
}

/// A query embedding and the model that made it.
#[derive(Clone, Debug, Deserialize)]
pub struct QueryEmbedding {
    pub model: String,
    pub vector: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Hybrid,
    Lexical,
    Vector,
}

/// `knowledge.search`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SearchInput {
    pub query: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub collection: Option<String>,
    /// Only these documents.
    #[serde(default)]
    pub documents: Vec<String>,
    #[serde(default)]
    pub k: Option<usize>,
    #[serde(default)]
    pub max_sensitivity: Option<Sensitivity>,
    #[serde(default)]
    pub embedding: Option<QueryEmbedding>,
    /// Stop adding passages once their characters would pass this.
    #[serde(default)]
    pub budget_chars: Option<usize>,
    #[serde(default)]
    pub mode: SearchMode,
}

#[derive(Clone, Debug, Serialize)]
pub struct PassageHit {
    pub passage_id: String,
    pub document_id: String,
    pub document_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    pub sensitivity: Sensitivity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub section_path: String,
    pub kind: PassageKind,
    pub content: String,
    /// "Report.pdf, p. 12 › Revenue › By region".
    pub citation: String,
    /// 0 to 1, higher is better; compares within one search only.
    pub score: f64,
    pub signals: Value,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct KnowledgeStats {
    pub documents: usize,
    pub passages: usize,
    /// Passages with an embedding, by model.
    pub embedded: BTreeMap<String, usize>,
}

/// One collection and what it holds.
#[derive(Clone, Debug, Serialize)]
pub struct CollectionInfo {
    pub name: String,
    pub documents: usize,
    pub passages: usize,
}

/// Lookup structures, kept under one lock.
#[derive(Default)]
struct State {
    docs: HashMap<String, Document>,
    passages: HashMap<String, Passage>,
    /// Passage ids of each document, by ordinal.
    by_doc: HashMap<String, BTreeMap<u32, String>>,
    text: TextIndex,
    vectors: VectorIndex,
    /// Embeddings by model, by passage id (unit length).
    embedded: HashMap<String, HashMap<String, Vec<f32>>>,
}

impl State {
    fn add_passage(&mut self, p: Passage) {
        let name = self.docs.get(&p.document).map(|d| d.name.clone()).unwrap_or_default();
        self.remove_passage(&p.id);
        self.text.upsert(&p.id, &format!("{} {}", p.section_path, p.content));
        self.vectors.upsert(&p.id, &p.embed_text(&name));
        if let Some((model, v)) = p.embedding() {
            self.embedded.entry(model.to_string()).or_default().insert(p.id.clone(), unit(v));
        }
        self.by_doc.entry(p.document.clone()).or_default().insert(p.ordinal, p.id.clone());
        self.passages.insert(p.id.clone(), p);
    }

    fn remove_passage(&mut self, id: &str) -> Option<Passage> {
        let p = self.passages.remove(id)?;
        self.text.remove(id);
        self.vectors.remove(id);
        for by_id in self.embedded.values_mut() {
            by_id.remove(id);
        }
        if let Some(ords) = self.by_doc.get_mut(&p.document) {
            if ords.get(&p.ordinal).is_some_and(|x| x == id) {
                ords.remove(&p.ordinal);
            }
            if ords.is_empty() {
                self.by_doc.remove(&p.document);
            }
        }
        Some(p)
    }

    fn remove_document(&mut self, id: &str) {
        self.docs.remove(id);
        let ids: Vec<String> = self.by_doc.get(id).map(|o| o.values().cloned().collect()).unwrap_or_default();
        for p in ids {
            self.remove_passage(&p);
        }
    }

    /// The document as `p` may read it, with the ceiling `cap`.
    fn readable(&self, p: &Principal, id: &str, cap: Sensitivity) -> Option<&Document> {
        self.docs.get(id).filter(|d| may_read(p, d, cap))
    }
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

/// Whether `p` may read document `d` below the ceiling `cap`.
fn may_read(p: &Principal, d: &Document, cap: Sensitivity) -> bool {
    if !p.allows(Access::Read) || !p.in_workspace(&d.workspace) || d.sensitivity > cap {
        return false;
    }
    if d.acl.is_empty() || p.is_privileged() {
        return true;
    }
    let me = p.entity.as_deref().unwrap_or(&p.id);
    d.acl.iter().any(|a| a == me || a == &p.id)
}

fn doc_key(id: &str) -> String {
    format!("d/{id}")
}

fn passage_key(doc: &str, ordinal: u32) -> String {
    format!("p/{doc}/{ordinal:06}")
}

fn check_id(what: &str, id: &str) -> Result<()> {
    let ok = !id.is_empty() && id.len() <= 200 && !id.contains('/') && !id.chars().any(char::is_control);
    if ok { Ok(()) } else { Err(Error::invalid(format!("{what} id: 1 to 200 characters, no '/'"))) }
}

fn sha256_hex(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A passage's fused score, and its rank in each ranking (1-based).
type Fused = (f64, Option<usize>, Option<usize>);

/// A document's passages as the input gives them.
struct Cut {
    passages: Vec<Passage>,
    /// Cut at [`PASSAGES_MAX`].
    truncated: bool,
    pages: Option<u32>,
    embedding_model: Option<String>,
    /// Of the content as it came in.
    sha: String,
}

/// Whether giving `d` sensitivity `s` and `acl` lets more principals read
/// it: a lower sensitivity, or an ACL that admits someone the old one did
/// not (removing a non-empty one, or adding to it). Setting an ACL on a
/// document without one, or narrowing one, only takes readers away.
fn widens(d: &Document, s: Sensitivity, acl: &[String]) -> bool {
    let wider_acl = !d.acl.is_empty() && (acl.is_empty() || acl.iter().any(|a| !d.acl.contains(a)));
    s < d.sensitivity || wider_acl
}

/// The same document again: content and what describes it.
fn same_content(a: &Document, b: &Document) -> bool {
    a.sha256 == b.sha256
        && a.name == b.name
        && a.collection == b.collection
        && a.sensitivity == b.sensitivity
        && a.acl == b.acl
        && a.media_type == b.media_type
        && a.source == b.source
        && a.meta == b.meta
}

/// Documents and their passages. Cheap to clone and share; other
/// processes' writes to a shared store arrive through store notices.
#[derive(Clone)]
pub struct Knowledge {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn Store>,
    cfg: KnowledgeConfig,
    state: RwLock<State>,
    /// Serialises writes within this process.
    write: parking_lot::Mutex<()>,
}

impl Knowledge {
    pub fn open(store: Arc<dyn Store>, cfg: KnowledgeConfig) -> Result<Self> {
        let inner = Arc::new(Inner { store, cfg, state: RwLock::new(State::default()), write: Default::default() });
        inner.reload_all()?;
        let weak = Arc::downgrade(&inner);
        inner.store.subscribe(Arc::new(move |n: StoreNotice| {
            let Some(inner) = weak.upgrade() else { return };
            let done = match n {
                StoreNotice::Kv { ns, key, .. } if ns == NS => inner.reload(&key),
                StoreNotice::Resync => inner.reload_all(),
                _ => Ok(()),
            };
            if let Err(e) = done {
                tracing::warn!("knowledge: reloading after another process's write: {e}");
            }
        }))?;
        Ok(Self { inner })
    }

    fn cap(&self, p: &Principal, asked: Option<Sensitivity>) -> Sensitivity {
        let mut cap = asked.unwrap_or(Sensitivity::Restricted);
        if p.is_agent() {
            cap = cap.min(self.inner.cfg.agent_max_sensitivity);
        }
        cap
    }

    /// Add a document, or replace the one with the input's id.
    pub fn ingest(&self, p: &Principal, default_ws: &str, input: IngestInput) -> Result<IngestResult> {
        let ws = input.workspace.clone().filter(|w| !w.is_empty()).unwrap_or_else(|| default_ws.to_string());
        if !p.allows(Access::Write) || !p.in_workspace(&ws) {
            return Err(Error::Denied(format!("{} may not add documents to workspace {ws}", p.id)));
        }
        let name = input.name.trim().to_string();
        if name.is_empty() || name.chars().count() > 255 {
            return Err(Error::invalid("name: 1 to 255 characters"));
        }
        if let Some(id) = &input.id {
            check_id("document", id)?;
        }
        let Cut { mut passages, truncated, pages, embedding_model, sha } = self.cut(&input)?;
        let _w = self.inner.write.lock();
        let id = input.id.clone().unwrap_or_else(|| new_id("doc"));
        let now = now_ms();
        let mut doc = Document {
            id: id.clone(),
            workspace: ws.clone(),
            name,
            collection: input.collection.filter(|c| !c.trim().is_empty()),
            sensitivity: input.sensitivity.unwrap_or_default(),
            acl: input.acl,
            sha256: sha,
            media_type: input.media_type,
            source: input.source,
            pages,
            passages: passages.len(),
            truncated,
            embedding_model,
            created_by: p.id.clone(),
            created_at: now,
            updated_at: now,
            meta: input.meta,
        };
        let replaced = {
            let st = self.inner.state.read();
            match input.id.as_deref().and_then(|id| st.docs.get(id)) {
                Some(d) if !may_read(p, d, Sensitivity::Restricted) || d.workspace != ws => {
                    return Err(Error::Denied(format!("{} may not replace document {id}", p.id)));
                }
                Some(d) if same_content(d, &doc) => {
                    return Ok(IngestResult { document: d.clone(), created: false, replaced: false });
                }
                Some(d) if widens(d, doc.sensitivity, &doc.acl) && !p.is_privileged() => {
                    return Err(Error::Denied(format!(
                        "{} may not replace document {id} with a lower sensitivity or another acl (admins can)",
                        p.id
                    )));
                }
                Some(d) => {
                    doc.created_at = d.created_at;
                    doc.created_by = d.created_by.clone();
                    true
                }
                None if input.id.is_none() => {
                    // The same content is kept once per workspace.
                    let same = st.docs.values().find(|d| d.workspace == ws && d.sha256 == doc.sha256);
                    if let Some(d) = same.filter(|d| may_read(p, d, Sensitivity::Restricted)) {
                        return Ok(IngestResult { document: d.clone(), created: false, replaced: false });
                    }
                    false
                }
                None => false,
            }
        };
        for ps in &mut passages {
            ps.document = id.clone();
            if ps.id.is_empty() {
                ps.id = format!("{id}#{}", ps.ordinal);
            }
        }
        {
            let st = self.inner.state.read();
            for ps in &passages {
                if let Some(other) = st.passages.get(&ps.id)
                    && other.document != id
                {
                    return Err(Error::invalid(format!("passage id {} belongs to document {}", ps.id, other.document)));
                }
            }
        }
        let store = &self.inner.store;
        let old: Vec<u32> =
            self.inner.state.read().by_doc.get(&id).map(|o| o.keys().copied().collect()).unwrap_or_default();
        for ps in &passages {
            store.kv_put(NS, &passage_key(&id, ps.ordinal), &serde_json::to_value(ps)?, None)?;
        }
        for ord in old.iter().filter(|o| **o as usize >= passages.len()) {
            store.kv_delete(NS, &passage_key(&id, *ord))?;
        }
        store.kv_put(NS, &doc_key(&id), &serde_json::to_value(&doc)?, None)?;
        let mut st = self.inner.state.write();
        st.remove_document(&id);
        st.docs.insert(id.clone(), doc.clone());
        for ps in passages {
            st.add_passage(ps);
        }
        Ok(IngestResult { document: doc, created: true, replaced })
    }

    /// The input's passages, without ids or document yet.
    fn cut(&self, input: &IngestInput) -> Result<Cut> {
        let given = [input.text.is_some(), input.blocks.is_some(), input.passages.is_some()];
        if given.iter().filter(|g| **g).count() != 1 {
            return Err(Error::invalid("give one of text, blocks or passages"));
        }
        let max_chars = self.inner.cfg.max_document_chars;
        if let Some(text) = &input.text {
            if text.chars().count() > max_chars {
                return Err(Error::invalid(format!("text: at most {max_chars} characters")));
            }
            let (blocks, pages) = parse_text(text);
            let (chunks, truncated) = chunk(&blocks);
            let sha = sha256_hex(&["text", text]);
            return Ok(Cut { passages: passages_of(chunks), truncated, pages, embedding_model: None, sha });
        }
        if let Some(blocks) = &input.blocks {
            let chars: usize = blocks.iter().map(|b| b.text.chars().count()).sum();
            if chars > max_chars {
                return Err(Error::invalid(format!("blocks: at most {max_chars} characters")));
            }
            let (chunks, truncated) = chunk(blocks);
            let sha = sha256_hex(&["blocks", &serde_json::to_string(&chunks).unwrap_or_default()]);
            let pages = input.pages.or_else(|| blocks.iter().filter_map(|b| b.page).max());
            return Ok(Cut { passages: passages_of(chunks), truncated, pages, embedding_model: None, sha });
        }
        let given = input.passages.as_deref().unwrap_or_default();
        let model = input.embedding_model.as_deref().map(str::trim).filter(|m| !m.is_empty());
        if given.iter().any(|g| g.embedding.is_some()) && model.is_none() {
            return Err(Error::invalid("passages with an embedding need embedding_model"));
        }
        if model.is_some_and(|m| m.len() > 200) {
            return Err(Error::invalid("embedding_model: at most 200 characters"));
        }
        let mut dims = None;
        let mut out = Vec::new();
        let mut hasher = Sha256::new();
        for (n, g) in given.iter().take(PASSAGES_MAX).enumerate() {
            let content = g.content.trim();
            if content.is_empty() {
                return Err(Error::invalid(format!("passages[{n}]: content is empty")));
            }
            if content.chars().count() > PASSAGE_MAX_CHARS * 4 {
                return Err(Error::invalid(format!("passages[{n}]: at most {} characters", PASSAGE_MAX_CHARS * 4)));
            }
            if let Some(id) = &g.id {
                check_id("passage", id)?;
            }
            hasher.update(content.as_bytes());
            hasher.update([0]);
            let mut ps = Passage {
                id: g.id.clone().unwrap_or_default(),
                document: String::new(),
                ordinal: n as u32,
                page: g.page,
                section_path: g.section_path.clone().unwrap_or_default(),
                kind: g.kind.unwrap_or_default(),
                content: content.to_string(),
                embedding: None,
            };
            if let (Some(v), Some(m)) = (&g.embedding, model) {
                if v.is_empty() || v.iter().any(|x| !x.is_finite()) {
                    return Err(Error::invalid(format!("passages[{n}]: an embedding is finite numbers")));
                }
                if *dims.get_or_insert(v.len()) != v.len() {
                    return Err(Error::invalid(format!("passages[{n}]: embeddings of one model have one length")));
                }
                ps.set_embedding(m, v);
            }
            out.push(ps);
        }
        let mut seen = std::collections::HashSet::new();
        for ps in &out {
            if !ps.id.is_empty() && !seen.insert(ps.id.clone()) {
                return Err(Error::invalid(format!("passage id {} is given twice", ps.id)));
            }
        }
        if let Some(m) = model {
            hasher.update(m.as_bytes());
        }
        let sha = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok(Cut {
            passages: out,
            truncated: given.len() > PASSAGES_MAX,
            pages: input.pages,
            embedding_model: model.map(String::from),
            sha,
        })
    }

    /// The passages that best answer `input.query`.
    pub fn search(&self, p: &Principal, input: &SearchInput) -> Result<Vec<PassageHit>> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let ws = input.workspace.clone().filter(|w| !w.is_empty());
        if let Some(w) = &ws
            && !p.in_workspace(w)
        {
            return Err(Error::Denied(format!("{} has no access to workspace {w}", p.id)));
        }
        let k = input.k.unwrap_or(5).clamp(1, 100);
        let query = input.query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let cap = self.cap(p, input.max_sensitivity);
        let st = self.inner.state.read();
        let keep = |pid: &str| {
            let Some(ps) = st.passages.get(pid) else { return false };
            let Some(d) = st.readable(p, &ps.document, cap) else { return false };
            ws.as_ref().is_none_or(|w| &d.workspace == w)
                && input.collection.as_ref().is_none_or(|c| d.collection.as_ref() == Some(c))
                && (input.documents.is_empty() || input.documents.contains(&d.id))
        };
        let lexical: Vec<String> = if input.mode == SearchMode::Vector {
            Vec::new()
        } else {
            st.text.search(query, CANDIDATES, keep).into_iter().map(|(id, _)| id).collect()
        };
        let semantic: Vec<String> = match (input.mode, &input.embedding) {
            (SearchMode::Lexical, _) => Vec::new(),
            (_, Some(q)) => {
                let qv = unit(q.vector.clone());
                let mut scored: Vec<(String, f32)> = st
                    .embedded
                    .get(&q.model)
                    .map(|by_id| {
                        by_id
                            .iter()
                            .filter(|(id, v)| v.len() == qv.len() && keep(id))
                            .map(|(id, v)| (id.clone(), cosine(v, &qv)))
                            .filter(|(_, s)| *s >= MIN_EMBEDDING_SIMILARITY)
                            .collect()
                    })
                    .unwrap_or_default();
                scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                scored.truncate(CANDIDATES);
                scored.into_iter().map(|(id, _)| id).collect()
            }
            (_, None) => st
                .vectors
                .search(query, CANDIDATES, MIN_HASHED_SIMILARITY, keep)
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        };
        let lists = usize::from(!lexical.is_empty()) + usize::from(!semantic.is_empty());
        if lists == 0 {
            return Ok(Vec::new());
        }
        let mut fused: HashMap<&str, Fused> = HashMap::new();
        for (rank, id) in lexical.iter().enumerate() {
            let slot = fused.entry(id.as_str()).or_default();
            slot.0 += 1.0 / (RRF_K + 1.0 + rank as f64);
            slot.1 = Some(rank + 1);
        }
        for (rank, id) in semantic.iter().enumerate() {
            let slot = fused.entry(id.as_str()).or_default();
            slot.0 += 1.0 / (RRF_K + 1.0 + rank as f64);
            slot.2 = Some(rank + 1);
        }
        // The best a passage can score: first in both rankings.
        let best = 2.0 / (RRF_K + 1.0);
        let mut ranked: Vec<(&str, Fused)> = fused.into_iter().collect();
        ranked.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
        let semantic_signal = if input.embedding.is_some() { "embedding_rank" } else { "vector_rank" };
        let mut out = Vec::new();
        let mut used = 0usize;
        for (id, (score, lr, sr)) in ranked {
            if out.len() >= k {
                break;
            }
            let ps = &st.passages[id];
            let d = &st.docs[&ps.document];
            let chars = ps.content.chars().count();
            if let Some(budget) = input.budget_chars
                && !out.is_empty()
                && used + chars > budget
            {
                break;
            }
            used += chars;
            out.push(PassageHit {
                passage_id: ps.id.clone(),
                document_id: d.id.clone(),
                document_name: d.name.clone(),
                collection: d.collection.clone(),
                sensitivity: d.sensitivity,
                page: ps.page,
                section_path: ps.section_path.clone(),
                kind: ps.kind,
                content: ps.content.clone(),
                citation: citation(d, ps),
                score: ((score / best) * 1000.0).round() / 1000.0,
                signals: Value::Object(
                    [("lexical_rank".to_string(), json!(lr)), (semantic_signal.to_string(), json!(sr))]
                        .into_iter()
                        .collect(),
                ),
            });
        }
        Ok(out)
    }

    /// Documents `p` may read, newest first.
    pub fn list(
        &self,
        p: &Principal,
        workspace: Option<&str>,
        collection: Option<&str>,
        limit: usize,
    ) -> Vec<Document> {
        let cap = self.cap(p, None);
        let st = self.inner.state.read();
        let mut docs: Vec<Document> = st
            .docs
            .values()
            .filter(|d| may_read(p, d, cap))
            .filter(|d| workspace.is_none_or(|w| d.workspace == w))
            .filter(|d| collection.is_none_or(|c| d.collection.as_deref() == Some(c)))
            .cloned()
            .collect();
        docs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
        docs.truncate(limit.clamp(1, 1000));
        docs
    }

    /// A document `p` may read, with its passages when asked.
    pub fn get(&self, p: &Principal, id: &str, with_passages: bool) -> Result<Value> {
        let cap = self.cap(p, None);
        let st = self.inner.state.read();
        let d = st.readable(p, id, cap).ok_or_else(|| Error::not_found(id))?;
        let mut v = json!({ "document": d });
        if with_passages {
            let ps: Vec<&Passage> = st
                .by_doc
                .get(id)
                .map(|o| o.values().filter_map(|pid| st.passages.get(pid)).collect())
                .unwrap_or_default();
            v["passages"] = serde_json::to_value(ps)?;
        }
        Ok(v)
    }

    /// Rename, regroup, relabel or re-scope a document. Lowering its
    /// sensitivity or changing its ACL widens who reads it: admins only.
    pub fn update(&self, p: &Principal, mut input: UpdateInput) -> Result<Document> {
        if input.set_acl {
            input.acl = Some(input.acl.unwrap_or_default());
        }
        if input.clear_collection {
            input.collection = Some(String::new());
        }
        let _w = self.inner.write.lock();
        let mut d = {
            let st = self.inner.state.read();
            st.readable(p, &input.id, Sensitivity::Restricted).cloned().ok_or_else(|| Error::not_found(&input.id))?
        };
        if !p.allows(Access::Write) {
            return Err(Error::Denied(format!("{} may not change documents", p.id)));
        }
        let to = (input.sensitivity.unwrap_or(d.sensitivity), input.acl.as_ref().unwrap_or(&d.acl));
        if widens(&d, to.0, to.1) && !p.is_privileged() {
            return Err(Error::Denied(format!(
                "{} may not lower a document's sensitivity or widen its acl (admins can)",
                p.id
            )));
        }
        if let Some(n) = input.name.map(|n| n.trim().to_string()) {
            if n.is_empty() || n.chars().count() > 255 {
                return Err(Error::invalid("name: 1 to 255 characters"));
            }
            d.name = n;
        }
        if let Some(c) = input.collection {
            d.collection = Some(c).filter(|c| !c.trim().is_empty());
        }
        if let Some(s) = input.sensitivity {
            d.sensitivity = s;
        }
        if let Some(a) = input.acl {
            d.acl = a;
        }
        if let Some(m) = input.meta {
            d.meta = m;
        }
        d.updated_at = now_ms();
        self.inner.store.kv_put(NS, &doc_key(&d.id), &serde_json::to_value(&d)?, None)?;
        let mut st = self.inner.state.write();
        let renamed = st.docs.get(&d.id).is_some_and(|o| o.name != d.name);
        st.docs.insert(d.id.clone(), d.clone());
        if renamed {
            // The hashed vectors embed the document's name.
            let ps: Vec<Passage> = st
                .by_doc
                .get(&d.id)
                .map(|o| o.values().filter_map(|id| st.passages.get(id).cloned()).collect())
                .unwrap_or_default();
            for ps in ps {
                st.add_passage(ps);
            }
        }
        Ok(d)
    }

    /// Delete a document and its passages.
    pub fn delete(&self, p: &Principal, id: &str) -> Result<bool> {
        let _w = self.inner.write.lock();
        let ords: Vec<u32> = {
            let st = self.inner.state.read();
            if st.readable(p, id, Sensitivity::Restricted).is_none() {
                return Ok(false);
            }
            st.by_doc.get(id).map(|o| o.keys().copied().collect()).unwrap_or_default()
        };
        if !p.allows(Access::Delete) {
            return Err(Error::Denied(format!("{} may not delete documents", p.id)));
        }
        // The document first: other processes stop serving its passages at once.
        self.inner.store.kv_delete(NS, &doc_key(id))?;
        for ord in ords {
            self.inner.store.kv_delete(NS, &passage_key(id, ord))?;
        }
        self.inner.state.write().remove_document(id);
        Ok(true)
    }

    pub fn collections(&self, p: &Principal, workspace: Option<&str>) -> Vec<CollectionInfo> {
        let cap = self.cap(p, None);
        let st = self.inner.state.read();
        let mut by: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for d in st.docs.values().filter(|d| may_read(p, d, cap) && workspace.is_none_or(|w| d.workspace == w)) {
            if let Some(c) = &d.collection {
                let e = by.entry(c.clone()).or_default();
                e.0 += 1;
                e.1 += d.passages;
            }
        }
        by.into_iter().map(|(name, (documents, passages))| CollectionInfo { name, documents, passages }).collect()
    }

    pub fn stats(&self) -> KnowledgeStats {
        let st = self.inner.state.read();
        KnowledgeStats {
            documents: st.docs.len(),
            passages: st.passages.len(),
            embedded: st.embedded.iter().filter(|(_, v)| !v.is_empty()).map(|(m, v)| (m.clone(), v.len())).collect(),
        }
    }
}

impl Inner {
    fn reload_all(&self) -> Result<()> {
        let mut st = State::default();
        let mut passages = Vec::new();
        for (key, v) in self.store.kv_scan(NS, "")? {
            if key.starts_with("d/") {
                if let Ok(d) = serde_json::from_value::<Document>(v) {
                    st.docs.insert(d.id.clone(), d);
                }
            } else if key.starts_with("p/")
                && let Ok(ps) = serde_json::from_value::<Passage>(v)
            {
                passages.push(ps);
            }
        }
        // Passages of a document whose row is missing (an ingest that did not
        // finish, a delete in progress) are not served.
        passages.retain(|ps| st.docs.contains_key(&ps.document));
        for ps in passages {
            st.add_passage(ps);
        }
        *self.state.write() = st;
        Ok(())
    }

    /// Another process wrote or deleted `key`.
    fn reload(&self, key: &str) -> Result<()> {
        let fresh = self.store.kv_get(NS, key)?;
        let mut st = self.state.write();
        if let Some(id) = key.strip_prefix("d/") {
            match fresh.and_then(|v| serde_json::from_value::<Document>(v).ok()) {
                Some(d) => {
                    let renamed = st.docs.get(id).is_none_or(|o| o.name != d.name);
                    st.docs.insert(id.to_string(), d);
                    if renamed {
                        let ps: Vec<Passage> = st
                            .by_doc
                            .get(id)
                            .map(|o| o.values().filter_map(|p| st.passages.get(p).cloned()).collect())
                            .unwrap_or_default();
                        for ps in ps {
                            st.add_passage(ps);
                        }
                    }
                }
                None => st.remove_document(id),
            }
        } else if let Some((doc, ord)) = key.strip_prefix("p/").and_then(|r| r.split_once('/')) {
            let ord: u32 = ord.parse().unwrap_or(u32::MAX);
            let old = st.by_doc.get(doc).and_then(|o| o.get(&ord)).cloned();
            if let Some(old) = old {
                st.remove_passage(&old);
            }
            if let Some(ps) = fresh.and_then(|v| serde_json::from_value::<Passage>(v).ok()) {
                st.add_passage(ps);
            }
        }
        Ok(())
    }
}

/// "Report.pdf, p. 12 › Revenue › By region": where a reader finds the passage.
fn citation(d: &Document, ps: &Passage) -> String {
    let mut out = d.name.clone();
    if let Some(page) = ps.page {
        out.push_str(&format!(", p. {page}"));
    }
    if !ps.section_path.is_empty() {
        out.push_str(&format!(" › {}", ps.section_path));
    }
    out
}

// ── parsing and chunking ───────────────────────────────────────────────

/// A passage ready to be stored (no id or document yet).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Chunk {
    pub page: Option<u32>,
    pub section_path: String,
    pub kind: PassageKind,
    pub content: String,
}

fn passages_of(chunks: Vec<Chunk>) -> Vec<Passage> {
    chunks
        .into_iter()
        .enumerate()
        .map(|(n, c)| Passage {
            id: String::new(),
            document: String::new(),
            ordinal: n as u32,
            page: c.page,
            section_path: c.section_path,
            kind: c.kind,
            content: c.content,
            embedding: None,
        })
        .collect()
}

/// Plain text or Markdown as blocks: `#` headings, `|` tables (a `|---|`
/// line after the first row makes it a header), paragraphs between blank
/// lines. A form feed starts a new page (as `pdftotext` writes them); the
/// page count is `None` without one.
pub fn parse_text(text: &str) -> (Vec<Block>, Option<u32>) {
    let paged = text.contains('\u{c}');
    let mut page = 1u32;
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut table: Vec<&str> = Vec::new();
    fn flush_para(para: &mut Vec<&str>, page: Option<u32>, out: &mut Vec<Block>) {
        if !para.is_empty() {
            out.push(Block { kind: BlockKind::Text, text: para.join(" "), page, ..Default::default() });
            para.clear();
        }
    }
    fn flush_table(table: &mut Vec<&str>, page: Option<u32>, out: &mut Vec<Block>) {
        if table.is_empty() {
            return;
        }
        let is_rule = |l: &str| {
            let t = l.trim().trim_matches('|');
            !t.is_empty() && t.chars().all(|c| matches!(c, '-' | ':' | '|' | ' '))
        };
        let header = table.len() > 1 && is_rule(table[1]);
        let rows: Vec<Vec<String>> = table
            .iter()
            .enumerate()
            .filter(|(i, l)| !(header && *i == 1) && !is_rule(l))
            .map(|(_, l)| {
                let t = l.trim();
                let t = t.strip_prefix('|').unwrap_or(t);
                let t = t.strip_suffix('|').unwrap_or(t);
                t.split('|').map(|c| c.trim().to_string()).collect()
            })
            .collect();
        out.push(Block {
            kind: BlockKind::Table,
            page,
            rows: Some(rows),
            header_rows: Some(usize::from(header)),
            ..Default::default()
        });
        table.clear();
    }
    for (n, chunk) in text.split('\u{c}').enumerate() {
        if n > 0 {
            flush_para(&mut para, paged.then_some(page), &mut blocks);
            flush_table(&mut table, paged.then_some(page), &mut blocks);
            page += 1;
        }
        let pg = paged.then_some(page);
        for line in chunk.lines() {
            let t = line.trim();
            if t.starts_with('|') {
                flush_para(&mut para, pg, &mut blocks);
                table.push(t);
                continue;
            }
            flush_table(&mut table, pg, &mut blocks);
            let hashes = t.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
                flush_para(&mut para, pg, &mut blocks);
                blocks.push(Block {
                    kind: BlockKind::Heading,
                    level: Some(hashes as u8),
                    text: t[hashes..].trim().to_string(),
                    page: pg,
                    ..Default::default()
                });
            } else if t.is_empty() {
                flush_para(&mut para, pg, &mut blocks);
            } else {
                para.push(t);
            }
        }
        flush_para(&mut para, pg, &mut blocks);
        flush_table(&mut table, pg, &mut blocks);
    }
    (blocks, paged.then_some(page))
}

fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cuts `text` into pieces of at most `max` characters, at sentence ends or
/// spaces where it can.
fn split_long(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > max {
        let cut = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        let window = &rest[..cut];
        let at = window
            .rfind(". ")
            .map(|i| i + 1)
            .filter(|i| *i > cut / 2)
            .or_else(|| window.rfind(' ').filter(|i| *i > cut / 2))
            .unwrap_or(cut);
        out.push(rest[..at].trim().to_owned());
        rest = rest[at..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    out
}

/// The label of each column: its header cells from top to bottom, joined
/// ("2024 / Q1"); an empty header cell under a merged one takes the value
/// to its left.
fn column_labels(rows: &[Vec<String>], header_rows: usize, width: usize) -> Vec<String> {
    let mut labels = vec![String::new(); width];
    for row in rows.iter().take(header_rows) {
        let mut carried = String::new();
        for (col, label) in labels.iter_mut().enumerate() {
            let cell = squeeze(row.get(col).map_or("", String::as_str));
            let cell = if cell.is_empty() {
                carried.clone()
            } else {
                carried.clone_from(&cell);
                cell
            };
            if !cell.is_empty() && !label.split(" / ").any(|part| part == cell) {
                if !label.is_empty() {
                    label.push_str(" / ");
                }
                label.push_str(&cell);
            }
        }
    }
    labels
}

/// A table as passages: each data row one line of `label: value` pairs.
fn table_chunks(block: &Block, section: &str, out: &mut Vec<Chunk>) {
    let Some(rows) = block.rows.as_deref().filter(|r| !r.is_empty()) else { return };
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return;
    }
    let header_rows = block.header_rows.unwrap_or(1).min(rows.len().saturating_sub(1));
    let labels = column_labels(rows, header_rows, width);
    let mut current = String::new();
    let flush = |current: &mut String, out: &mut Vec<Chunk>| {
        if !current.trim().is_empty() {
            out.push(Chunk {
                page: block.page,
                section_path: section.to_owned(),
                kind: PassageKind::Table,
                content: std::mem::take(current).trim_end().to_owned(),
            });
        }
    };
    for row in rows.iter().skip(header_rows) {
        let cells: Vec<String> = (0..width)
            .filter_map(|col| {
                let value = squeeze(row.get(col).map_or("", String::as_str));
                if value.is_empty() {
                    return None;
                }
                Some(match labels[col].as_str() {
                    "" => value,
                    label => format!("{label}: {value}"),
                })
            })
            .collect();
        if cells.is_empty() {
            continue;
        }
        let mut line = cells.join("; ");
        if line.chars().count() > PASSAGE_MAX_CHARS {
            line = line.chars().take(PASSAGE_MAX_CHARS).collect();
        }
        if !current.is_empty() && current.chars().count() + line.chars().count() > PASSAGE_TARGET_CHARS {
            flush(&mut current, out);
        }
        current.push_str(&line);
        current.push('\n');
    }
    flush(&mut current, out);
}

/// Splits blocks into passages: text gathers under its headings up to
/// [`PASSAGE_TARGET_CHARS`], a heading starts a new passage, and tables
/// become row-wise passages. At most [`PASSAGES_MAX`]; the flag says
/// whether the document was cut there.
pub fn chunk(blocks: &[Block]) -> (Vec<Chunk>, bool) {
    let mut out = Vec::new();
    let mut headings: Vec<(u8, String)> = Vec::new();
    let mut section = String::new();
    let mut current = String::new();
    let mut page = None;
    fn flush(current: &mut String, page: Option<u32>, section: &str, out: &mut Vec<Chunk>) {
        if !current.trim().is_empty() {
            out.push(Chunk {
                page,
                section_path: section.to_owned(),
                kind: PassageKind::Text,
                content: std::mem::take(current).trim().to_owned(),
            });
        }
        current.clear();
    }
    for block in blocks {
        if out.len() > PASSAGES_MAX {
            break;
        }
        match block.kind {
            BlockKind::Heading => {
                flush(&mut current, page, &section, &mut out);
                let text = squeeze(&block.text);
                if text.is_empty() {
                    continue;
                }
                let level = block.level.unwrap_or(1).clamp(1, 6);
                headings.retain(|(l, _)| *l < level);
                headings.push((level, text.chars().take(120).collect()));
                section = headings.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" › ");
            }
            BlockKind::Text => {
                let text = squeeze(&block.text);
                if text.is_empty() {
                    continue;
                }
                for piece in split_long(&text, PASSAGE_MAX_CHARS) {
                    if !current.is_empty() && current.chars().count() + piece.chars().count() > PASSAGE_TARGET_CHARS {
                        flush(&mut current, page, &section, &mut out);
                    }
                    if current.is_empty() {
                        page = block.page;
                    } else {
                        current.push('\n');
                    }
                    current.push_str(&piece);
                }
            }
            BlockKind::Table => {
                flush(&mut current, page, &section, &mut out);
                table_chunks(block, &section, &mut out);
            }
        }
    }
    flush(&mut current, page, &section, &mut out);
    let cut = out.len() > PASSAGES_MAX;
    out.truncate(PASSAGES_MAX);
    (out, cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onto_core::policy::Role;
    use onto_core::store::MemoryStore;

    fn kb() -> (Knowledge, Arc<dyn Store>) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        (Knowledge::open(store.clone(), KnowledgeConfig::default()).unwrap(), store)
    }

    fn member() -> Principal {
        Principal::member("ann", "acme")
    }

    fn text(name: &str, text: &str) -> IngestInput {
        IngestInput { name: name.into(), text: Some(text.into()), ..Default::default() }
    }

    fn search(k: &Knowledge, p: &Principal, q: &str) -> Vec<PassageHit> {
        k.search(p, &SearchInput { query: q.into(), ..Default::default() }).unwrap()
    }

    const HANDBOOK: &str = "# Handbook\n\nWelcome to the team.\n\n## Expenses\n\nTravel is reimbursed within 30 days \
         of the receipt.\n\n## Deployments\n\nWe deploy the payment API on Tuesdays after the freeze.\n\n\
         | Region | 2024 | 2025 |\n|---|---|---|\n| EMEA | 10 | 12 |\n| APAC | 7 | 9 |\n";

    #[test]
    fn markdown_becomes_sections_and_row_wise_tables() {
        let (blocks, pages) = parse_text(HANDBOOK);
        assert_eq!(pages, None);
        let (chunks, cut) = chunk(&blocks);
        assert!(!cut);
        let by_section: Vec<(&str, PassageKind)> = chunks.iter().map(|c| (c.section_path.as_str(), c.kind)).collect();
        assert_eq!(
            by_section,
            [
                ("Handbook", PassageKind::Text),
                ("Handbook › Expenses", PassageKind::Text),
                ("Handbook › Deployments", PassageKind::Text),
                ("Handbook › Deployments", PassageKind::Table),
            ]
        );
        assert_eq!(chunks[3].content, "Region: EMEA; 2024: 10; 2025: 12\nRegion: APAC; 2024: 7; 2025: 9");
        // A form feed starts a page.
        let (blocks, pages) = parse_text("first page\u{c}second page");
        assert_eq!(pages, Some(2));
        assert_eq!(blocks.iter().map(|b| b.page).collect::<Vec<_>>(), [Some(1), Some(2)]);
    }

    #[test]
    fn long_text_is_cut_near_the_target_and_never_past_the_limit() {
        let sentence = "The payment API retries a failed capture three times before it gives up. ";
        let blocks = vec![Block { kind: BlockKind::Text, text: sentence.repeat(200), ..Default::default() }];
        let (chunks, _) = chunk(&blocks);
        assert!(chunks.len() > 5);
        for c in &chunks {
            let n = c.content.chars().count();
            assert!(n <= PASSAGE_MAX_CHARS, "{n}");
        }
        assert!(chunks[..chunks.len() - 1].iter().all(|c| c.content.chars().count() >= PASSAGE_TARGET_CHARS / 2));
        // Two-row headers name columns from top to bottom.
        let table = Block {
            kind: BlockKind::Table,
            rows: Some(vec![
                vec!["".into(), "2024".into(), "".into()],
                vec!["Region".into(), "Q1".into(), "Q2".into()],
                vec!["EMEA".into(), "1".into(), "2".into()],
            ]),
            header_rows: Some(2),
            ..Default::default()
        };
        let (chunks, _) = chunk(&[table]);
        assert_eq!(chunks[0].content, "Region: EMEA; 2024 / Q1: 1; 2024 / Q2: 2");
    }

    #[test]
    fn a_search_finds_passages_by_words_and_by_form_with_citations() {
        let (k, _) = kb();
        let r = k.ingest(&member(), "acme", text("Handbook.md", HANDBOOK)).unwrap();
        assert!(r.created);
        assert_eq!(r.document.passages, 4);
        let hits = search(&k, &member(), "travel receipt reimbursed");
        assert_eq!(hits[0].section_path, "Handbook › Expenses");
        assert_eq!(hits[0].citation, "Handbook.md › Handbook › Expenses");
        assert!(hits[0].score > 0.0 && hits[0].score <= 1.0, "{}", hits[0].score);
        assert_eq!(hits[0].passage_id, format!("{}#1", r.document.id));
        // Hashed vectors match other forms of the words ("deployments" ~ "deploy").
        let v = k
            .search(
                &member(),
                &SearchInput { query: "deployment".into(), mode: SearchMode::Vector, ..Default::default() },
            )
            .unwrap();
        assert_eq!(v[0].section_path, "Handbook › Deployments", "{v:?}");
        // A table row is found by its values.
        let t = search(&k, &member(), "APAC 2025");
        assert_eq!(t[0].kind, PassageKind::Table);
        // A budget stops adding passages.
        let one = k
            .search(
                &member(),
                &SearchInput {
                    query: "team travel deploy region".into(),
                    budget_chars: Some(10),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn imported_passages_keep_their_ids_and_are_found_by_their_embeddings() {
        let (k, _) = kb();
        let passages = vec![
            PassageInput {
                id: Some("chunk-a".into()),
                content: "Customs paperwork for the port of Rotterdam".into(),
                embedding: Some(vec![1.0, 0.0, 0.0]),
                ..Default::default()
            },
            PassageInput {
                id: Some("chunk-b".into()),
                content: "The cafeteria menu changes weekly".into(),
                embedding: Some(vec![0.0, 1.0, 0.0]),
                ..Default::default()
            },
        ];
        let input = IngestInput {
            id: Some("doc-1".into()),
            name: "Logistics.pdf".into(),
            passages: Some(passages),
            embedding_model: Some("text-embedding-3-small".into()),
            ..Default::default()
        };
        let r = k.ingest(&member(), "acme", input).unwrap();
        assert_eq!(r.document.embedding_model.as_deref(), Some("text-embedding-3-small"));
        // By meaning alone: the query shares no word with the passage.
        let q = SearchInput {
            query: "shipping documents".into(),
            embedding: Some(QueryEmbedding { model: "text-embedding-3-small".into(), vector: vec![0.9, 0.1, 0.0] }),
            ..Default::default()
        };
        let hits = k.search(&member(), &q).unwrap();
        assert_eq!(hits[0].passage_id, "chunk-a");
        assert_eq!(hits[0].signals["embedding_rank"], 1);
        // Another model's vectors do not compare.
        let other = SearchInput {
            embedding: Some(QueryEmbedding { model: "other".into(), vector: vec![1.0, 0.0, 0.0] }),
            mode: SearchMode::Vector,
            ..q
        };
        assert!(k.search(&member(), &other).unwrap().is_empty());
        assert_eq!(k.stats().embedded["text-embedding-3-small"], 2);
        // An embedding without its model is refused, and so are mixed lengths.
        let bad = IngestInput {
            name: "x".into(),
            passages: Some(vec![PassageInput {
                content: "x".into(),
                embedding: Some(vec![1.0]),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert!(k.ingest(&member(), "acme", bad).is_err());
    }

    #[test]
    fn readers_see_what_their_workspace_sensitivity_and_acl_allow() {
        let (k, _) = kb();
        let add = |name: &str, s: Sensitivity, acl: Vec<String>| {
            let input =
                IngestInput { sensitivity: Some(s), acl, ..text(name, &format!("quarterly revenue report {name}")) };
            k.ingest(&Principal::admin("root"), "acme", input).unwrap().document
        };
        add("public", Sensitivity::Public, vec![]);
        add("internal", Sensitivity::Internal, vec![]);
        add("confidential", Sensitivity::Confidential, vec![]);
        add("board", Sensitivity::Restricted, vec!["user:cfo".into()]);
        let names = |p: &Principal, max: Option<Sensitivity>| {
            let q = SearchInput {
                query: "quarterly revenue".into(),
                k: Some(10),
                max_sensitivity: max,
                ..Default::default()
            };
            let mut n: Vec<String> = k.search(p, &q).unwrap().into_iter().map(|h| h.document_name).collect();
            n.sort();
            n
        };
        assert_eq!(names(&member(), None), ["confidential", "internal", "public"], "the ACL keeps the board pack");
        assert_eq!(names(&member(), Some(Sensitivity::Internal)), ["internal", "public"]);
        let cfo = Principal::new("user:cfo", vec![Role::Viewer], vec!["acme".into()]);
        assert_eq!(names(&cfo, None), ["board", "confidential", "internal", "public"]);
        // Agents stop at [knowledge] agent_max_sensitivity (internal), whatever they ask.
        let agent = Principal::new("agent:a", vec![Role::Agent], vec!["acme".into()]);
        assert_eq!(names(&agent, Some(Sensitivity::Restricted)), ["internal", "public"]);
        assert_eq!(k.list(&agent, None, None, 10).len(), 2);
        // Other workspaces see nothing; naming one they lack is refused.
        let outsider = Principal::member("eve", "other");
        assert!(names(&outsider, None).is_empty());
        let q = SearchInput { query: "revenue".into(), workspace: Some("acme".into()), ..Default::default() };
        assert!(k.search(&outsider, &q).is_err());
        // Viewers read but do not write.
        let viewer = Principal::viewer("val", "acme");
        assert!(k.ingest(&viewer, "acme", text("x", "y")).is_err());
    }

    #[test]
    fn the_same_content_is_kept_once_and_an_id_replaces() {
        let (k, store) = kb();
        let first = k.ingest(&member(), "acme", text("a.md", HANDBOOK)).unwrap();
        let again = k.ingest(&member(), "acme", text("copy.md", HANDBOOK)).unwrap();
        assert!(!again.created);
        assert_eq!(again.document.id, first.document.id);
        // Replacing with fewer passages removes the others, from the index and the store.
        let id = first.document.id.clone();
        let short = IngestInput { id: Some(id.clone()), ..text("a.md", "Only one paragraph now.") };
        let r = k.ingest(&member(), "acme", short).unwrap();
        assert!(r.replaced);
        assert_eq!(r.document.passages, 1);
        assert_eq!(r.document.created_at, first.document.created_at);
        assert!(search(&k, &member(), "travel reimbursed").is_empty());
        assert_eq!(store.kv_scan(NS, &format!("p/{id}/")).unwrap().len(), 1);
        // Lowering sensitivity widens who reads: admins only. Raising it is anyone's.
        let up = UpdateInput { id: id.clone(), sensitivity: Some(Sensitivity::Confidential), ..Default::default() };
        assert_eq!(k.update(&member(), up).unwrap().sensitivity, Sensitivity::Confidential);
        let down = UpdateInput { id: id.clone(), sensitivity: Some(Sensitivity::Public), ..Default::default() };
        assert!(matches!(k.update(&member(), down.clone()), Err(Error::Denied(_))));
        // Nor can a member widen it by replacing the document.
        let wider =
            IngestInput { id: Some(id.clone()), sensitivity: Some(Sensitivity::Internal), ..text("a.md", "x y") };
        assert!(matches!(k.ingest(&member(), "acme", wider), Err(Error::Denied(_))));
        assert_eq!(k.update(&Principal::admin("root"), down).unwrap().sensitivity, Sensitivity::Public);
        // An ACL can be set or narrowed by a member, not removed or widened.
        let set = UpdateInput { id: id.clone(), acl: Some(vec!["ann".into(), "bob".into()]), ..Default::default() };
        assert!(k.update(&member(), set).is_ok());
        let narrower = UpdateInput { id: id.clone(), acl: Some(vec!["ann".into()]), ..Default::default() };
        assert!(k.update(&member(), narrower).is_ok());
        let wider = UpdateInput { id: id.clone(), acl: Some(vec!["ann".into(), "eve".into()]), ..Default::default() };
        assert!(matches!(k.update(&member(), wider), Err(Error::Denied(_))));
        let cleared = UpdateInput { id: id.clone(), set_acl: true, ..Default::default() };
        assert!(matches!(k.update(&member(), cleared), Err(Error::Denied(_))));
        let cleared = UpdateInput { id: id.clone(), set_acl: true, ..Default::default() };
        assert!(k.update(&Principal::admin("root"), cleared).is_ok());
        // Delete removes every row.
        assert!(k.delete(&member(), &id).unwrap());
        assert!(store.kv_scan(NS, "").unwrap().is_empty());
        assert!(!k.delete(&member(), &id).unwrap());
    }

    #[test]
    fn documents_survive_a_reopen() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        let a = Knowledge::open(store.clone(), KnowledgeConfig::default()).unwrap();
        let doc = a.ingest(&member(), "acme", text("Handbook.md", HANDBOOK)).unwrap().document;
        let reopened = Knowledge::open(store.clone(), KnowledgeConfig::default()).unwrap();
        assert_eq!(reopened.stats().passages, 4);
        assert_eq!(search(&reopened, &member(), "travel receipt")[0].document_id, doc.id);
        // A passage row without its document row (an ingest that did not
        // finish) is not served.
        store.kv_delete(NS, &doc_key(&doc.id)).unwrap();
        let partial = Knowledge::open(store, KnowledgeConfig::default()).unwrap();
        assert_eq!((partial.stats().documents, partial.stats().passages), (0, 0));
    }

    #[test]
    fn collections_group_documents() {
        let (k, _) = kb();
        for (name, c) in [("a", "hr"), ("b", "hr"), ("c", "ops")] {
            let input = IngestInput { collection: Some(c.into()), ..text(name, &format!("policy text {name}")) };
            k.ingest(&member(), "acme", input).unwrap();
        }
        let cs = k.collections(&member(), None);
        assert_eq!(cs.iter().map(|c| (c.name.as_str(), c.documents)).collect::<Vec<_>>(), [("hr", 2), ("ops", 1)]);
        let q =
            SearchInput { query: "policy".into(), collection: Some("ops".into()), k: Some(10), ..Default::default() };
        assert_eq!(k.search(&member(), &q).unwrap().len(), 1);
    }
}
