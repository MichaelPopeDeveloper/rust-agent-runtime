//! `agent-runtime`: an HTTP service that runs Claude Code agents.
//!
//! Two backends behind one small API:
//!
//! - **Agent runs** (`/agent/run`, `/agent/stream`) drive the full Claude Code
//!   harness — built-in tools, MCP, hooks, permissions — through the
//!   [`claude_agent_sdk`] crate in this workspace. Authentication is whatever
//!   the `claude` CLI is logged in with.
//! - **Direct Messages API** (`/messages`, `/messages/complete`) calls
//!   `POST /v1/messages` with an API key, for plain completions with no agent
//!   loop. This is the path the original scaffold started on, made correct.
//!
//! The library exposes the router and config so the server can be driven in
//! tests without binding a port; `main.rs` is a thin wrapper.

pub mod api;
pub mod config;
pub mod error;
pub mod routes;
pub mod sessions;
pub mod warm;

pub use config::Config;
pub use routes::{ApiJson, AppState, Stats, app, build};
