//! # onto-core
//!
//! The ontology layer of onto-intelli.
//!
//! * **Things** (workspace, team, project, issue, task, knowledge, artifact,
//!   conversation, ...) are typed entities in an in-memory graph backed by a
//!   durable [`store::Store`].
//! * **Events** (who did what, when, with which result) go to a separate event
//!   store and reference things by id. They never become graph nodes, so the
//!   graph stays small and fast while the audit trail can grow without bound.
//! * **Provenance** records where artifacts and knowledge came from.
//! * Every mutation runs through [`ActionContext`]: actor + workspace +
//!   authorization + target + execution + result, and is audited.
//!
//! All reads are synchronous and take microseconds; callers that need async
//! ingestion wrap [`Ontology::apply`] in a queue (see the `onto-intelli` crate).

pub mod action_types;
pub mod deps;
pub mod engine;
pub mod error;
pub mod experts;
pub mod export;
pub mod graph;
pub mod metrics;
pub mod model;
pub mod policy;
pub mod profile;
pub mod quality;
pub mod query;
pub mod rules;
pub mod schema;
pub mod similarity;
pub mod store;
pub mod text;
pub mod time;

pub use engine::{
    ActionContext, ChangeSet, ChangeSummary, HistoryEntry, MergeResult, OntoStats, Ontology, OntologyOptions,
    SearchHit, SearchMode,
};
pub use error::{Error, Result};
pub use model::*;
pub use policy::{Access, Principal, Role};
pub use schema::{Dir, Schema};
