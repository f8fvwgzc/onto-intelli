//! One LISTEN connection per pool, shared by every tenant store on it.
//!
//! A LISTEN holds its connection for as long as it listens. With one per
//! tenant, a pool of 10 connections served at most 10 open tenants, and the
//! eleventh hung waiting for a connection. Every commit now also notifies
//! [`SHARED_CHANNEL`] with the tenant's schema in the payload; this listener
//! is the pool's only LISTEN, and it hands each notice to the stores of that
//! schema. (The per-schema channel is still notified, for replicas of older
//! versions during a rolling upgrade.)

use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::Value;
use sqlx::postgres::{PgListener, PgPool};
use tokio::runtime::Handle;

use crate::sqlx;
use crate::store::StoreNotice;

/// The channel every commit notifies, whatever its tenant.
pub const SHARED_CHANNEL: &str = "onto_intelli";

struct Sub {
    id: u64,
    /// The store's own process id: its own commits are not echoed back.
    origin: String,
    tx: mpsc::Sender<StoreNotice>,
}

#[derive(Default)]
struct State {
    /// Subscribers by tenant schema.
    subs: HashMap<String, Vec<Sub>>,
    next: u64,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl State {
    fn send_all(&mut self, notice: &StoreNotice) {
        for subs in self.subs.values_mut() {
            subs.retain(|s| s.tx.send(notice.clone()).is_ok());
        }
    }

    fn dispatch(&mut self, payload: &str) {
        let Ok(v) = serde_json::from_str::<Value>(payload) else { return };
        let Some(schema) = v["s"].as_str() else { return };
        let Some(subs) = self.subs.get_mut(schema) else { return };
        let origin = v["o"].as_str().unwrap_or_default();
        let notice = match v["t"].as_str() {
            Some("kv") => StoreNotice::Kv {
                ns: v["ns"].as_str().unwrap_or_default().into(),
                key: v["key"].as_str().unwrap_or_default().into(),
                origin: origin.into(),
            },
            _ => StoreNotice::Commit { seq: v["seq"].as_u64().unwrap_or(0), origin: origin.into() },
        };
        subs.retain(|s| s.origin == origin || s.tx.send(notice.clone()).is_ok());
    }

    fn listening(&self) -> usize {
        self.subs.values().map(Vec::len).sum()
    }
}

/// The pool's listener and the stores subscribed to it.
pub struct Notifier {
    pool: PgPool,
    state: Arc<Mutex<State>>,
}

impl Drop for Notifier {
    fn drop(&mut self) {
        if let Some(t) = self.state.lock().task.take() {
            t.abort();
        }
    }
}

impl Notifier {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self { pool, state: Arc::new(Mutex::new(State::default())) })
    }

    /// Notices for `schema` from other processes go to `tx` until the
    /// returned subscription is dropped. The listener starts on `handle`
    /// with the first subscription.
    pub fn subscribe(
        self: &Arc<Self>,
        handle: &Handle,
        schema: &str,
        origin: &str,
        tx: mpsc::Sender<StoreNotice>,
    ) -> Subscription {
        let mut st = self.state.lock();
        st.next += 1;
        let id = st.next;
        st.subs.entry(schema.to_string()).or_default().push(Sub { id, origin: origin.to_string(), tx });
        if st.task.as_ref().is_none_or(|t| t.is_finished()) {
            st.task = Some(handle.spawn(listen(self.pool.clone(), self.state.clone())));
        }
        Subscription { notifier: self.clone(), schema: schema.to_string(), id }
    }

    /// Stores subscribed right now (all tenants).
    pub fn subscribers(&self) -> usize {
        self.state.lock().listening()
    }
}

/// Unsubscribes when dropped.
pub struct Subscription {
    notifier: Arc<Notifier>,
    schema: String,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut st = self.notifier.state.lock();
        if let Some(subs) = st.subs.get_mut(&self.schema) {
            subs.retain(|s| s.id != self.id);
            if subs.is_empty() {
                st.subs.remove(&self.schema);
            }
        }
    }
}

async fn listen(pool: PgPool, state: Arc<Mutex<State>>) {
    loop {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(l) => l,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if listener.listen(SHARED_CHANNEL).await.is_err() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        // Anything committed while nobody listened is caught up now.
        state.lock().send_all(&StoreNotice::Resync);
        loop {
            match listener.try_recv().await {
                Ok(Some(n)) => state.lock().dispatch(n.payload()),
                // Connection lost (and re-established): notices may be missing.
                Ok(None) => state.lock().send_all(&StoreNotice::Resync),
                Err(_) => break,
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(schema: &str, origin: &str) -> (State, mpsc::Receiver<StoreNotice>) {
        let (tx, rx) = mpsc::channel();
        let mut st = State::default();
        st.subs.entry(schema.into()).or_default().push(Sub { id: 1, origin: origin.into(), tx });
        (st, rx)
    }

    #[test]
    fn notices_reach_the_stores_of_their_schema_only() {
        let (mut st, rx) = state_with("onto_a", "proc_1");
        st.dispatch(r#"{"s":"onto_b","t":"c","seq":3,"o":"proc_2"}"#);
        assert!(rx.try_recv().is_err(), "another tenant's commit");
        st.dispatch(r#"{"s":"onto_a","t":"c","seq":4,"o":"proc_1"}"#);
        assert!(rx.try_recv().is_err(), "the store's own commit is not echoed");
        st.dispatch(r#"{"s":"onto_a","t":"c","seq":5,"o":"proc_2"}"#);
        assert!(matches!(rx.try_recv(), Ok(StoreNotice::Commit { seq: 5, .. })));
        st.dispatch(r#"{"s":"onto_a","t":"kv","ns":"x","key":"k","o":"proc_2"}"#);
        assert!(matches!(rx.try_recv(), Ok(StoreNotice::Kv { ref ns, .. }) if ns == "x"));
        st.dispatch("not json");
        st.send_all(&StoreNotice::Resync);
        assert!(matches!(rx.try_recv(), Ok(StoreNotice::Resync)));
    }

    #[test]
    fn closed_subscribers_are_dropped() {
        let (mut st, rx) = state_with("onto_a", "proc_1");
        drop(rx);
        st.dispatch(r#"{"s":"onto_a","t":"c","seq":5,"o":"proc_2"}"#);
        assert_eq!(st.listening(), 0);
    }
}
