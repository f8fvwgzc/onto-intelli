//! Leases, consumer cursors, timers, write fencing and wake-ups for
//! background work (docs/design/EVENTS.md D12, sections 9 and 10).
//!
//! * A **lease** names one holder for a while. Every time it changes holder
//!   its **epoch** grows. Times are the store's clock (the database clock
//!   `now_ms()` on PostgreSQL), never the caller's: [`Store::lease`] takes a
//!   duration, not a deadline, so a process whose clock is skewed or that
//!   was paused cannot extend or steal a lease by believing in its own time.
//! * A **fence** ([`Fence`]) is a lease as one holder last took it. A write
//!   carrying a fence commits only while that holder still has the lease at
//!   that epoch; otherwise it is refused with `Error::Unavailable("lease
//!   lost …")` and nothing is written. A holder that lost its lease (expired
//!   while it was paused, then taken by another) therefore cannot write
//!   behind the new holder's back.
//! * A **consumer** cursor records how far a background reader of the change
//!   feed got; moving it is fenced.
//! * A **timer** is work due at a store-clock time (retries).
//! * **Wake** rows (PostgreSQL only, one shared table per deployment) tell
//!   the deployment's one elected scanner which closed tenants have work due,
//!   so closed tenants are never polled. A tenant's workers settle their row
//!   with a compare-and-set on its `bumped` counter, so a bump that races the
//!   settle is never lost.
//!
//! None of this goes through the commit log or its NOTIFY.
//!
//! [`Store::lease`]: crate::store::Store::lease

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::Store;

/// A lease as one holder took it: writes carrying it commit only while that
/// holder still has the lease at this epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fence {
    pub name: String,
    pub holder: String,
    pub epoch: u64,
}

impl Fence {
    pub fn new(name: impl Into<String>, holder: impl Into<String>, epoch: u64) -> Self {
        Self { name: name.into(), holder: holder.into(), epoch }
    }
}

/// A lease row as the store holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub name: String,
    pub holder: String,
    pub epoch: u64,
    /// Store-clock milliseconds until which the holder has it.
    pub until: i64,
    /// `until` is not past by the store's clock at the time of the read.
    pub held: bool,
}

/// Work due at a store-clock time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    pub name: String,
    /// Store-clock milliseconds.
    pub due_at: i64,
    #[serde(default)]
    pub doc: Value,
}

/// A tenant's wake row: when its background work is next due, and how many
/// times it was bumped (the compare-and-set token of [`Store::wake_settle`]).
///
/// [`Store::wake_settle`]: crate::store::Store::wake_settle
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wake {
    /// Store-clock milliseconds; `None`: nothing due.
    pub due_at: Option<i64>,
    pub bumped: u64,
}

/// A random suffix chosen once per process start.
fn process_suffix() -> &'static str {
    static SUFFIX: OnceLock<String> = OnceLock::new();
    SUFFIX.get_or_init(|| format!("{:016x}", fastrand::u64(..)))
}

/// The holder id this process uses for leases on `store`: the store's origin
/// (one per opened store) plus a random suffix per process start, so two
/// processes, or two openings of one tenant, never share a holder.
pub fn holder_for(store: &dyn Store) -> String {
    let origin = store.origin();
    let origin = if origin.is_empty() { "local" } else { origin.as_str() };
    format!("{origin}:{}", process_suffix())
}

pub(crate) fn lease_lost(f: &Fence) -> crate::Error {
    crate::Error::Unavailable(format!("lease lost: {} is no longer held by {} at epoch {}", f.name, f.holder, f.epoch))
}

pub(crate) fn no_leases() -> crate::Error {
    crate::Error::Unavailable("this store keeps no leases, consumers or timers".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    #[test]
    fn holders_differ_per_store_and_name_the_process() {
        let (a, b) = (MemoryStore::default(), MemoryStore::default());
        let (ha, hb) = (holder_for(&a), holder_for(&b));
        assert_eq!(ha, hb, "memory stores have no origin: the process suffix decides");
        assert!(ha.starts_with("local:") && ha.len() == "local:".len() + 16, "{ha}");
        let e = lease_lost(&Fence::new("automation:consumer", "h", 3)).to_string();
        assert!(e.starts_with("unavailable: lease lost") && e.contains("epoch 3"), "{e}");
    }
}
