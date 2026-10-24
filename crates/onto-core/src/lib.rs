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
pub mod authority;
pub mod catalog;
pub mod deps;
pub mod engine;
pub mod error;
pub mod execution;
pub mod experts;
pub mod explorer;
pub mod export;
pub mod graph;
pub mod learn;
pub mod lint;
pub mod live_schema;
pub mod locks;
pub mod map;
pub mod metrics;
pub mod model;
pub mod modeling;
pub mod objects;
pub mod policy;
pub mod profile;
pub mod proposals;
pub mod quality;
pub mod query;
pub mod reason;
pub mod rules;
pub mod schema;
pub mod shape_model;
pub mod shapes;
pub mod similarity;
pub mod store;
pub mod text;
pub mod time;
pub mod trace;

pub use engine::{
    ActionContext, ChangeSet, ChangeSummary, HistoryEntry, MergeResult, OntoStats, Ontology, OntologyOptions,
    SearchHit, SearchMode, SyncReport,
};
pub use error::{Error, Result};
pub use lint::{LintDecision, LintFeedback, LintQuery, LintReport};
pub use live_schema::{
    Consequences, Conservativity, Governance, LikelyRename, Migration, Risk, SchemaApplied, SchemaChange,
    SchemaChangeSet, SchemaDiff, SchemaMode, SchemaPlan, SchemaVersion, SchemaVersionInfo,
};
pub use locks::{LockViolation, SchemaLock};
pub use model::*;
pub use policy::{Access, Principal, Role};
pub use proposals::{Decision, ProposalInput, ProposalStatus, ProposalView, SchemaProposal};
pub use reason::{
    Attested, BindingReport, BindingVerdict, Certificate, Closure, ClosureDiff, Derivation, Fact, QueryAnswer,
    ReasonOptions, ReasonStats, Status as FactStatus,
};
pub use schema::{Dir, Schema};
pub use shape_model::{PropertyShape, Shape, ShapeSeverity};
pub use shapes::{
    InduceOptions, InducedShape, ShapeCheck, ShapeDelta, ShapeIssue, ShapeIssueKind, ShapeReport, ShapeResult,
    ShapeVerdict, ShapesQuery, SkippedConstraint,
};
/// The sqlx 0.9 API the PostgreSQL store is built on, as `sqlx::…` paths.
/// An application's `sqlx::PgPool` is the same type as `sqlx::PgPool` here:
/// hand it to `store::PgShared::from_pool`.
#[cfg(feature = "postgres")]
pub mod sqlx {
    pub use sqlx_core::*;
    pub use sqlx_core::{
        query::query, query_as::query_as, query_scalar::query_scalar, raw_sql::raw_sql, row::Row,
        sql_str::AssertSqlSafe,
    };
    pub use sqlx_postgres as postgres;
    pub use sqlx_postgres::{PgPool, Postgres};
}
