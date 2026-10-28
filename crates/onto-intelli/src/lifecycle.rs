//! Stopping a tenant's background work for a server shutdown
//! (docs/design/EVENTS.md section 9, and docs/STORAGE.md, upgrades).
//!
//! A process that stops without giving its leases back leaves them held
//! until they expire (a minute for fan-out and the automation consumer, the
//! delivery claim lease for claimed outbox rows), so a replacement process
//! waits that long before changes are fanned out or runs start. On shutdown
//! a tenant therefore:
//!
//! 1. closes ([`Intelli::begin_shutdown`]): its change streams end with a
//!    final `end` frame (`tenant_closed`, with the cursor to resume after),
//!    and its workers start no new round and claim no new rows;
//! 2. waits, until a deadline, for the workers to end the round they are in
//!    (a delivery in flight finishes or is cut at the deadline);
//! 3. releases what this process holds: every lease a worker took, fenced
//!    by the epoch it took it at (a stale term releases nothing), the outbox
//!    rows it claimed and did not settle (fenced by their claim token; they
//!    are sent again by the next claim, at least once, never exactly once),
//!    and the crawl leases of crawls it started.
//!
//! Nothing is lost by releasing: consumer cursors stay where the fenced
//! writes left them, and runs claimed under an older epoch are recovered by
//! the next holder. A write a worker still makes after the release carries
//! the old fence and is refused ("lease lost").

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;

use onto_core::leases::Fence;

use crate::intelli::Intelli;

/// How long releasing may take once the workers stopped (store calls).
const RELEASE_BOUND: Duration = Duration::from_secs(5);

/// What a shutdown gave back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Released {
    /// Leases released (fan-out, automation consumer).
    pub leases: usize,
    /// Claimed outbox rows given back unsent.
    pub claims: usize,
    /// Crawl leases given back.
    pub crawls: usize,
    /// Workers still in a round at the deadline (cut; their later writes
    /// are refused by the released fences).
    pub cut: usize,
    /// Releases that failed (a store error); those expire on their own.
    pub failed: usize,
}

impl Released {
    pub fn add(&mut self, o: &Released) {
        self.leases += o.leases;
        self.claims += o.claims;
        self.crawls += o.crawls;
        self.cut += o.cut;
        self.failed += o.failed;
    }
}

/// What a tenant's background work holds in this process.
#[derive(Default)]
pub(crate) struct Holdings {
    /// The fence each lease was last taken with, by lease name.
    leases: Mutex<HashMap<String, Fence>>,
    /// Outbox rows claimed and not yet settled: id → claim token.
    claims: Mutex<HashMap<String, String>>,
    /// Crawls in progress: lease → holder.
    crawls: Mutex<HashMap<String, String>>,
    /// Workers that take leases or claim rows.
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Holdings {
    /// A worker took (or renewed) a lease.
    pub(crate) fn took(&self, f: &Fence) {
        self.leases.lock().insert(f.name.clone(), f.clone());
    }

    /// A worker found the lease held by another holder.
    pub(crate) fn lost(&self, name: &str) {
        self.leases.lock().remove(name);
    }

    pub(crate) fn claimed(&self, id: &str, claim: &str) {
        self.claims.lock().insert(id.to_string(), claim.to_string());
    }

    pub(crate) fn settled(&self, id: &str) {
        self.claims.lock().remove(id);
    }

    pub(crate) fn crawling(&self, lease: &str, holder: &str) {
        self.crawls.lock().insert(lease.to_string(), holder.to_string());
    }

    pub(crate) fn crawled(&self, lease: &str) {
        self.crawls.lock().remove(lease);
    }

    pub(crate) fn worker(&self, h: tokio::task::JoinHandle<()>) {
        let mut w = self.workers.lock();
        w.retain(|h| !h.is_finished());
        w.push(h);
    }

    /// The leases held now (tests and health).
    pub(crate) fn leases(&self) -> Vec<Fence> {
        let mut v: Vec<Fence> = self.leases.lock().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

impl Intelli {
    /// Close this tenant for a server shutdown: change streams end with
    /// their final frame, and workers start no new round. Returns at once;
    /// [`finish_shutdown`](Self::finish_shutdown) waits and releases.
    pub fn begin_shutdown(&self) {
        self.mark_stopping();
        self.close();
    }

    /// Wait until `deadline` for the workers to end their round, then
    /// release what this process holds (see the module documentation).
    pub async fn finish_shutdown(&self, deadline: tokio::time::Instant) -> Released {
        let h = self.holdings();
        let mut out = Released::default();
        let workers = std::mem::take(&mut *h.workers.lock());
        for mut w in workers {
            if tokio::time::timeout_at(deadline, &mut w).await.is_err() {
                w.abort();
                out.cut += 1;
            }
        }
        let leases: Vec<Fence> = h.leases.lock().drain().map(|(_, f)| f).collect();
        let claims: Vec<(String, String)> = h.claims.lock().drain().collect();
        let crawls: Vec<(String, String)> = h.crawls.lock().drain().collect();
        if leases.is_empty() && claims.is_empty() && crawls.is_empty() {
            return out;
        }
        let onto = self.ontology().clone();
        let job = tokio::task::spawn_blocking(move || {
            let store = onto.store();
            let mut r = Released::default();
            for f in &leases {
                match store.release(f) {
                    Ok(true) => r.leases += 1,
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(lease = %f.name, "releasing a lease on shutdown: {e}");
                        r.failed += 1;
                    }
                }
            }
            for (id, claim) in &claims {
                match store.outbox_release(id, claim) {
                    Ok(true) => r.claims += 1,
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(id = %id, "releasing a claimed outbox row on shutdown: {e}");
                        r.failed += 1;
                    }
                }
            }
            for (lease, holder) in &crawls {
                match onto.release_lease(lease, holder) {
                    Ok(()) => r.crawls += 1,
                    Err(e) => {
                        tracing::warn!(lease = %lease, "releasing a crawl lease on shutdown: {e}");
                        r.failed += 1;
                    }
                }
            }
            r
        });
        match tokio::time::timeout(RELEASE_BOUND, job).await {
            Ok(Ok(r)) => out.add(&r),
            Ok(Err(e)) => {
                tracing::warn!("releasing on shutdown: {e}");
                out.failed += 1;
            }
            Err(_) => {
                tracing::warn!(
                    "releasing on shutdown took longer than {RELEASE_BOUND:?}; the rest expire on their own"
                );
                out.failed += 1;
            }
        }
        out
    }

    /// [`begin_shutdown`](Self::begin_shutdown), then
    /// [`finish_shutdown`](Self::finish_shutdown) within `grace`.
    pub async fn shutdown(&self, grace: Duration) -> Released {
        self.begin_shutdown();
        self.finish_shutdown(tokio::time::Instant::now() + grace).await
    }

    /// The leases this tenant's workers hold in this process now.
    pub fn held_leases(&self) -> Vec<Fence> {
        self.holdings().leases()
    }
}
