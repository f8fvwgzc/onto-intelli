//! The deployment's one wake scanner (docs/design/EVENTS.md section 10).
//!
//! Tenants that have background work due (outbox rows, retries, changes for
//! enabled endpoints or automations) bump their row in the shared
//! `{prefix}_sys.wake` table at write time. Closed tenants are never polled:
//! one scanner per deployment, elected with a session advisory lock, wakes on
//! `LISTEN {prefix}_wake` or every poll interval, selects the rows due by the
//! database clock and hands the tenant ids to its caller (the hub opens them).
//!
//! The advisory lock lives on a connection of the scanner's own, outside the
//! pool, so it never takes a pool connection away from requests. When that
//! connection dies the lock is gone with it, the term ends, and another
//! process can be elected within one poll interval.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::watch;

use super::{PgPool, quote_ident, sql, undefined};
use crate::Result;
use crate::sqlx;
use crate::sqlx::connection::Connection;
use crate::sqlx::postgres::{PgConnectOptions, PgConnection, PgListener, PgPoolOptions};

/// Receives the tenants due at one scan.
pub type WakeFn = Arc<dyn Fn(Vec<String>) + Send + Sync>;

/// Tenants handed over per scan.
const SCAN_LIMIT: usize = 256;
/// After a notice, wait this long so a burst of bumps makes one scan.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// A running scanner candidate. Dropping it stops the candidate and, if it
/// was elected, releases the lock.
pub struct WakeScanner {
    pub(super) elected: Arc<AtomicBool>,
    pub(super) stop: watch::Sender<bool>,
}

impl WakeScanner {
    /// This process holds the deployment's scanner lock right now.
    pub fn elected(&self) -> bool {
        self.elected.load(Ordering::SeqCst)
    }
}

impl Drop for WakeScanner {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

pub(super) struct ScanJob {
    pub pool: PgPool,
    pub sys: String,
    pub lock_key: String,
    pub channel: String,
    pub poll: Duration,
    pub elected: Arc<AtomicBool>,
    pub on_due: WakeFn,
}

/// Tenants due by the database clock, earliest first (none while the wake
/// table does not exist yet).
pub(super) async fn due_tenants(pool: &PgPool, sys: &str, limit: usize) -> Result<Vec<String>> {
    let q = quote_ident(sys);
    let rows = sqlx::query_scalar::<_, String>(sql(format!(
        "SELECT tenant FROM {q}.wake WHERE due_at <= {q}.now_ms() ORDER BY due_at, tenant LIMIT $1"
    )))
    .bind(limit.clamp(1, 10_000) as i64)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(t) => Ok(t),
        Err(e) if undefined(&e) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Stand for election every poll interval until stopped.
pub(super) async fn scan(job: ScanJob, mut stop: watch::Receiver<bool>) {
    let opts: PgConnectOptions = (*job.pool.connect_options()).clone();
    loop {
        if *stop.borrow() {
            break;
        }
        let go_on = term(&job, &opts, &mut stop).await;
        job.elected.store(false, Ordering::SeqCst);
        if !go_on {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(job.poll) => {}
            _ = stop.changed() => break,
        }
    }
}

enum Woke {
    Notice,
    ListenerLost,
    Poll,
    Stop,
}

/// One attempt: take the lock, then scan until the lock connection fails or
/// the scanner stops. Returns false when stopped.
async fn term(job: &ScanJob, opts: &PgConnectOptions, stop: &mut watch::Receiver<bool>) -> bool {
    let Ok(mut lock) = PgConnection::connect_with(opts).await else { return true };
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
        .bind(&job.lock_key)
        .fetch_one(&mut lock)
        .await
        .unwrap_or(false);
    if !got {
        let _ = lock.close().await;
        return true;
    }
    job.elected.store(true, Ordering::SeqCst);
    // LISTEN on a one-connection pool of its own (outside the request pool).
    // Without it the scanner still polls.
    let listen_pool = PgPoolOptions::new()
        .max_connections(1)
        .max_lifetime(None)
        .idle_timeout(None)
        .connect_with(opts.clone())
        .await
        .ok();
    let mut listener = match &listen_pool {
        Some(p) => match PgListener::connect_with(p).await {
            Ok(mut l) => l.listen(&job.channel).await.is_ok().then_some(l),
            Err(_) => None,
        },
        None => None,
    };
    let mut go_on = true;
    loop {
        // The lock lives exactly as long as this session.
        if sqlx::query("SELECT 1").execute(&mut lock).await.is_err() {
            break;
        }
        if let Ok(due) = due_tenants(&job.pool, &job.sys, SCAN_LIMIT).await
            && !due.is_empty()
        {
            (job.on_due)(due);
        }
        let woke = {
            let notice = async {
                match listener.as_mut() {
                    // Ok(None): the listener reconnected and may have missed
                    // notices; scanning now covers them.
                    Some(l) => match l.try_recv().await {
                        Ok(_) => Woke::Notice,
                        Err(_) => Woke::ListenerLost,
                    },
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                w = notice => w,
                _ = tokio::time::sleep(job.poll) => Woke::Poll,
                _ = stop.changed() => Woke::Stop,
            }
        };
        match woke {
            Woke::Notice => {
                tokio::time::sleep(DEBOUNCE).await;
                if let Some(l) = listener.as_mut() {
                    while l.next_buffered().is_some() {}
                }
            }
            Woke::ListenerLost => listener = None,
            Woke::Poll => {}
            Woke::Stop => {
                go_on = false;
                break;
            }
        }
    }
    job.elected.store(false, Ordering::SeqCst);
    // Closing the session releases the advisory lock.
    let _ = lock.close().await;
    drop(listener);
    if let Some(p) = listen_pool {
        p.close().await;
    }
    go_on
}
