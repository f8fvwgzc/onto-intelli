//! PostgreSQL for integration tests. They run when `ONTO_TEST_DATABASE_URL`
//! or `DATABASE_URL` is set (`make db-up` starts one on port 5441) and are
//! skipped otherwise. Each test uses tenant schemas of its own and drops them.
#![allow(dead_code)]

use onto_intelli::core::store::{PgOptions, PgStore};

pub fn database_url(test: &str) -> Option<String> {
    let url = ["ONTO_TEST_DATABASE_URL", "DATABASE_URL"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|u| !u.trim().is_empty()));
    if url.is_none() {
        eprintln!("skipped {test}: set DATABASE_URL (or ONTO_TEST_DATABASE_URL) to run PostgreSQL tests");
    }
    url
}

/// A name no other test run uses.
pub fn unique(prefix: &str) -> String {
    format!("{prefix}{}", &onto_intelli::core::time::new_id("x")[2..12])
}

/// Drop `{schema_prefix}{tenant}` and everything in it.
pub fn drop_tenant(url: &str, schema_prefix: &str, tenant: &str) {
    let opts = PgOptions { schema_prefix: schema_prefix.into(), listen: false, ..Default::default() };
    if let Ok(store) = PgStore::connect(url, tenant, opts) {
        let _ = store.drop_tenant();
    }
}
