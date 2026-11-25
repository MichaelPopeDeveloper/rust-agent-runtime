//! Transport abstraction between the SDK and Claude Code.
//!
//! The default is [`SubprocessTransport`], which runs the `claude` CLI. The
//! [`Transport`] trait is public so callers can supply their own (e.g. a
//! remote connection, or a mock in tests), as the Python and TypeScript SDKs
//! allow. It is a low-level interface: the control protocol and message
//! routing are layered on top by the query engine.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;

use crate::errors::Result;

mod framing;
mod subprocess;

pub use subprocess::SubprocessTransport;

/// A stream of parsed JSON frames read from the transport.
pub type MessageStream = Pin<Box<dyn Stream<Item = Result<Value>> + Send>>;

/// Low-level transport for communicating with Claude Code.
///
/// Implementations move raw JSON frames in both directions. [`connect`] starts
/// the connection; [`take_message_stream`] hands out the single reader stream;
/// [`write`] sends one frame (callable concurrently with reading);
/// [`end_input`] closes the input side; [`close`] tears everything down.
///
/// [`connect`]: Transport::connect
/// [`take_message_stream`]: Transport::take_message_stream
/// [`write`]: Transport::write
/// [`end_input`]: Transport::end_input
/// [`close`]: Transport::close
#[async_trait]
pub trait Transport: Send + Sync {
    /// Start the connection. For subprocess transports, this spawns the
    /// process and begins reading its output.
    async fn connect(&mut self) -> Result<()>;

    /// Take the stream of incoming frames. Returns `None` if already taken.
    /// Call once, after [`connect`](Transport::connect).
    fn take_message_stream(&mut self) -> Option<MessageStream>;

    /// Write one raw frame (typically a JSON object plus a trailing newline).
    async fn write(&self, data: String) -> Result<()>;

    /// Close the input side (EOF on the subprocess's stdin).
    async fn end_input(&self) -> Result<()>;

    /// Close the connection and release resources. Must be safe to call more
    /// than once.
    async fn close(&self) -> Result<()>;

    /// Whether the transport is ready to send and receive.
    fn is_ready(&self) -> bool;
}
