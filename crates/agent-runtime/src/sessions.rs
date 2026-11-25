//! Persistent agent sessions.
//!
//! Spawning the `claude` CLI is the single most expensive thing this service
//! does: a large runtime cold-starts, then the `initialize` handshake runs,
//! and a fresh process per request also means a fresh conversation per
//! request. A [`SessionPool`] keeps a connected [`ClaudeSDKClient`] per
//! session id so follow-up turns reuse the live process and its context, and
//! reclaims sessions that sit idle past a TTL so idle processes don't pile up.
//!
//! A session's client sits behind a `tokio::sync::Mutex`; whoever holds the
//! guard owns the turn. The pool never waits for that lock: a locked session
//! is busy, so a second request for it is refused and the reaper skips it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use claude_agent_sdk::ClaudeSDKClient;
use tokio::sync::{Mutex, OwnedMutexGuard};

/// One live agent session: a connected client plus bookkeeping.
pub struct Session {
    /// The CLI's session id (known once the first turn completes).
    pub id: String,
    /// The connected client. Locked for the duration of a turn, so a session
    /// never runs two turns at once.
    pub client: Arc<Mutex<ClaudeSDKClient>>,
    last_used: StdMutex<Instant>,
    /// The model the session currently runs with (`None`: the CLI default),
    /// so a follow-up that asks for a different one can switch it.
    model: StdMutex<Option<String>>,
}

impl Session {
    pub fn new(
        id: String,
        client: Arc<Mutex<ClaudeSDKClient>>,
        model: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Session {
            id,
            client,
            last_used: StdMutex::new(Instant::now()),
            model: StdMutex::new(model),
        })
    }

    pub fn model(&self) -> Option<String> {
        self.model.lock().unwrap().clone()
    }

    pub fn set_model(&self, model: Option<String>) {
        *self.model.lock().unwrap() = model;
    }

    /// Reset the idle clock.
    pub fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }

    /// Claim the client for a turn without waiting. `None` means a turn is in
    /// progress (or the session is being closed).
    pub fn try_lock(&self) -> Option<OwnedMutexGuard<ClaudeSDKClient>> {
        self.client.clone().try_lock_owned().ok()
    }

    /// Disconnect the underlying client (terminates the CLI process), waiting
    /// for an in-flight turn to finish first.
    pub async fn close(&self) {
        self.client.lock().await.disconnect().await;
    }
}

/// Live sessions keyed by session id, with a cap and idle eviction.
pub struct SessionPool {
    sessions: StdMutex<HashMap<String, Arc<Session>>>,
    ttl: Duration,
    max: usize,
}

impl SessionPool {
    pub fn new(ttl: Duration, max: usize) -> Arc<Self> {
        Arc::new(SessionPool {
            sessions: StdMutex::new(HashMap::new()),
            ttl,
            max,
        })
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Keep `session` for follow-ups. Fails (and the caller should close the
    /// session) when the pool is full. Re-inserting an id replaces it.
    pub fn insert(&self, session: Arc<Session>) -> Result<(), PoolFull> {
        let mut sessions = self.sessions.lock().unwrap();
        if !sessions.contains_key(&session.id) && sessions.len() >= self.max {
            return Err(PoolFull(self.max));
        }
        sessions.insert(session.id.clone(), session);
        Ok(())
    }

    /// Remove exactly this session (not a newer one that reuses its id).
    pub fn remove_exact(&self, session: &Arc<Session>) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions
            .get(&session.id)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            sessions.remove(&session.id);
            true
        } else {
            false
        }
    }

    pub fn len(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ids of the live sessions with their idle time.
    pub fn snapshot(&self) -> Vec<(String, Duration)> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| (s.id.clone(), s.idle_for()))
            .collect()
    }

    /// Disconnect and drop every session idle longer than the TTL.
    pub async fn evict_idle(&self) -> usize {
        let candidates: Vec<Arc<Session>> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.idle_for() > self.ttl)
            .cloned()
            .collect();
        let mut evicted = 0;
        for session in candidates {
            // A locked session is mid-turn, so it is not idle whatever its
            // timestamp says. Holding the lock while closing also keeps a
            // racing request from claiming it (that request sees "busy").
            let Some(mut client) = session.try_lock() else {
                continue;
            };
            if session.idle_for() <= self.ttl || !self.remove_exact(&session) {
                continue;
            }
            client.disconnect().await;
            evicted += 1;
        }
        evicted
    }

    /// Disconnect every session (used on shutdown, so no CLI process is
    /// orphaned when the server exits). Sessions close concurrently, so this
    /// takes about as long as the slowest process exit, not the sum.
    pub async fn shutdown_all(&self) {
        let all: Vec<Arc<Session>> = self
            .sessions
            .lock()
            .unwrap()
            .drain()
            .map(|(_, s)| s)
            .collect();
        futures::future::join_all(all.iter().map(|session| session.close())).await;
    }

    /// Run idle eviction periodically in the background (needs a tokio
    /// runtime; without one, nothing is reaped and a warning is logged).
    pub fn spawn_reaper(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("no tokio runtime at startup; idle agent sessions will not be reaped");
            return;
        };
        let pool = self.clone();
        // Check often enough that a session outlives its TTL by at most 25%,
        // bounded so tiny TTLs (tests) and huge ones both behave.
        let period = (self.ttl / 4).clamp(Duration::from_millis(100), Duration::from_secs(30));
        handle.spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await; // the first tick completes immediately
            loop {
                ticker.tick().await;
                let evicted = pool.evict_idle().await;
                if evicted > 0 {
                    tracing::info!(evicted, live = pool.len(), "evicted idle agent sessions");
                }
            }
        });
    }
}

/// The pool has reached its configured maximum.
#[derive(Debug)]
pub struct PoolFull(pub usize);
