//! Pre-spawned `claude` processes for fresh runs.
//!
//! A fresh run pays for a process spawn and the `initialize` handshake before
//! its first token (600–800 ms measured against the real CLI). A [`WarmPool`]
//! keeps a few connected clients ready, built with the server's default
//! options, so a request that doesn't override those options starts at once;
//! the pool refills in the background. Spares are only warmed once the agent
//! routes are first used, so a deployment that never runs agents never pays
//! their memory.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use claude_agent_sdk::{ClaudeAgentOptions, ClaudeSDKClient};
use tokio_util::task::TaskTracker;

/// After a failed warm-up (CLI missing, not logged in, ...), wait this long
/// before trying again rather than retrying on every request.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(10);

/// Connected, idle clients ready to run a first turn.
pub struct WarmPool {
    options: ClaudeAgentOptions,
    target: usize,
    tasks: TaskTracker,
    inner: StdMutex<Inner>,
}

struct Inner {
    spares: Vec<ClaudeSDKClient>,
    /// Warm-ups in flight, so concurrent refills don't overshoot the target.
    filling: usize,
    cooldown_until: Option<Instant>,
    closed: bool,
}

impl WarmPool {
    /// A pool that keeps `target` spares built from `options`; its warm-up
    /// tasks are tracked by `tasks`.
    pub fn new(options: ClaudeAgentOptions, target: usize, tasks: TaskTracker) -> Arc<Self> {
        Arc::new(WarmPool {
            options,
            target,
            tasks,
            inner: StdMutex::new(Inner {
                spares: Vec::with_capacity(target),
                filling: 0,
                cooldown_until: None,
                closed: false,
            }),
        })
    }

    pub fn target(&self) -> usize {
        self.target
    }

    /// Spares ready right now.
    pub fn ready(&self) -> usize {
        self.inner.lock().unwrap().spares.len()
    }

    /// Take a ready spare, if any, and start refilling. A spare whose process
    /// has since exited is discarded rather than handed out.
    pub fn take(self: &Arc<Self>) -> Option<ClaudeSDKClient> {
        let spare = {
            let mut inner = self.inner.lock().unwrap();
            loop {
                match inner.spares.pop() {
                    None => break None,
                    Some(client) if client.is_connected() => break Some(client),
                    Some(dead) => {
                        tracing::warn!("discarding a warm spare whose process has exited");
                        drop(dead);
                    }
                }
            }
        };
        self.refill();
        spare
    }

    /// Start warm-ups in the background until `target` spares exist or are
    /// being started. Cheap when nothing is needed.
    pub fn refill(self: &Arc<Self>) {
        let deficit = {
            let mut inner = self.inner.lock().unwrap();
            if inner.closed
                || inner
                    .cooldown_until
                    .is_some_and(|until| until > Instant::now())
            {
                return;
            }
            let have = inner.spares.len() + inner.filling;
            let deficit = self.target.saturating_sub(have);
            inner.filling += deficit;
            deficit
        };
        for _ in 0..deficit {
            let pool = self.clone();
            self.tasks.spawn(async move { pool.warm_one().await });
        }
    }

    async fn warm_one(self: Arc<Self>) {
        let mut client = ClaudeSDKClient::new(self.options.clone());
        let result = client.connect().await;
        let leftover = {
            let mut inner = self.inner.lock().unwrap();
            inner.filling -= 1;
            match result {
                Ok(()) if !inner.closed => {
                    inner.spares.push(client);
                    tracing::debug!(ready = inner.spares.len(), "warmed a spare claude process");
                    None
                }
                Ok(()) => Some(client),
                Err(err) => {
                    inner.cooldown_until = Some(Instant::now() + FAILURE_COOLDOWN);
                    tracing::warn!(error = %err, "could not warm a spare claude process; will retry later");
                    None
                }
            }
        };
        if let Some(mut client) = leftover {
            // Warmed after shutdown began: don't leave it running.
            client.disconnect().await;
        }
    }

    /// Disconnect every spare (concurrently) and stop refilling.
    pub async fn shutdown_all(&self) {
        let spares = {
            let mut inner = self.inner.lock().unwrap();
            inner.closed = true;
            std::mem::take(&mut inner.spares)
        };
        futures::future::join_all(spares.into_iter().map(|mut client| async move {
            client.disconnect().await;
        }))
        .await;
    }
}
