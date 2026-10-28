//! # onto-intelli
//!
//! Enterprise Execution Intelligence on top of [`onto_core`]:
//!
//! ```text
//! question ─► intent ─► entity resolution ─► deterministic answer? ──yes──► answer (0 tokens)
//!                                   │ no
//!                                   ▼
//!        context graph (ontology + rules + root cause + events + knowledge + memory)
//!                                   ▼
//!             token-budgeted facts ─► LLM provider (cached) ─► answer
//!                                   ▼
//!             conversation + episodic memory + audit event
//! ```
//!
//! The orchestrator does the retrieval deterministically before the model is
//! called, so the LLM only reasons over a small, relevant, permission-filtered
//! context. Many questions never reach the LLM at all.
//!
//! * Async API: [`Intelli`]. Sync API: [`blocking::BlockingIntelli`].
//! * Every operation is also reachable by name through [`Intelli::call`]
//!   (JSON in, JSON out) — the same surface the gRPC, HTTP and C FFI layers use.

pub mod actions;
pub mod answer;
pub mod assurance;
pub mod automate;
pub mod blocking;
pub mod cache;
pub mod changes;
pub mod command;
pub mod config;
pub mod connectors;
pub mod context;
pub mod conversation;
pub mod eval;
pub mod hash;
pub mod hub;
pub mod ingest;
pub mod intelli;
pub mod intent;
pub mod limits;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod modeling;
pub mod net;
pub mod resolve;
pub mod seed;
pub mod token;
pub mod trace;
pub mod webhooks;
pub mod writeback;

pub use onto_core as core;
pub use onto_core::{
    ActionContext, Entity, EntityInput, Error, Event, EventInput, Id, Link, LinkInput, Mutation, Ontology, Principal,
    Result, Role, Schema,
};

pub use config::Config;
pub use hub::Hub;
pub use intelli::{AnswerMode, AskEvent, AskMode, AskRequest, AskResponse, Intelli};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
