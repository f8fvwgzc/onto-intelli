//! PostgreSQL for integration tests. They run when `ONTO_TEST_DATABASE_URL`
//! or `DATABASE_URL` is set (`make db-up` starts one on port 5441) and are
//! skipped otherwise. Every test gets a tenant schema of its own, dropped
//! when the [`TestTenant`] goes away.
#![allow(dead_code)]

use std::sync::Arc;

use onto_core::store::{PgOptions, PgStore};
use onto_core::{Ontology, OntologyOptions, Schema};

pub fn database_url() -> Option<String> {
    ["ONTO_TEST_DATABASE_URL", "DATABASE_URL"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|u| !u.trim().is_empty()))
}

pub struct TestTenant {
    pub url: String,
    pub tenant: String,
}

impl TestTenant {
    /// A fresh tenant, or `None` (and a note) when no test database is set.
    pub fn new(test: &str) -> Option<Self> {
        let Some(url) = database_url() else {
            eprintln!("skipped {test}: set DATABASE_URL (or ONTO_TEST_DATABASE_URL) to run PostgreSQL tests");
            return None;
        };
        let tenant = format!("test_{test}_{}", &onto_core::time::new_id("x")[2..12]);
        Some(Self { url, tenant })
    }

    pub fn store(&self) -> PgStore {
        PgStore::connect(&self.url, &self.tenant, PgOptions::default()).expect("connect to the test database")
    }

    pub fn open(&self, schema: Schema, opts: OntologyOptions) -> Ontology {
        Ontology::open(schema, Arc::new(self.store()), opts).expect("open the test tenant")
    }
}

impl Drop for TestTenant {
    fn drop(&mut self) {
        if let Ok(store) = PgStore::connect(&self.url, &self.tenant, PgOptions { listen: false, ..Default::default() })
        {
            let _ = store.drop_tenant();
        }
    }
}
