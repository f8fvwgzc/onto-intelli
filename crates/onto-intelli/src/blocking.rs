//! Synchronous API for callers without an async runtime (CLIs, scripts, FFI).
//!
//! Graph reads and writes are already synchronous through [`Ontology`]; this
//! wrapper adds a private Tokio runtime for the async parts (LLM, ingestion).
//! Do not use it from inside another Tokio runtime — use [`Intelli`] there.

use std::sync::Arc;

use serde_json::Value;
use tokio::runtime::{Builder, Runtime};

use onto_core::{Error, Ontology, Principal, Result};

use crate::config::Config;
use crate::intelli::{AskRequest, AskResponse, Intelli};
use crate::trace::TraceCtx;

pub struct BlockingIntelli {
    rt: Runtime,
    inner: Arc<Intelli>,
}

impl BlockingIntelli {
    pub fn open(config: Config) -> Result<Self> {
        let rt = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("onto-intelli")
            .enable_all()
            .build()
            .map_err(|e| Error::Other(format!("runtime: {e}")))?;
        let inner = rt.block_on(Intelli::open(config))?;
        Ok(Self { rt, inner })
    }

    pub fn in_memory() -> Result<Self> {
        Self::open(Config::in_memory())
    }

    pub fn ask(&self, p: &Principal, question: &str) -> Result<AskResponse> {
        self.ask_with(p, AskRequest::new(question))
    }

    pub fn ask_with(&self, p: &Principal, req: AskRequest) -> Result<AskResponse> {
        self.rt.block_on(self.inner.ask_traced(p, req, TraceCtx::from_source("ffi"))).result
    }

    pub fn call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        self.rt.block_on(self.inner.call_traced(p, action, input, TraceCtx::from_source("ffi"))).result
    }

    /// Direct synchronous graph access.
    pub fn ontology(&self) -> &Ontology {
        self.inner.ontology()
    }

    pub fn intelli(&self) -> &Arc<Intelli> {
        &self.inner
    }

    pub fn runtime(&self) -> &Runtime {
        &self.rt
    }
}
