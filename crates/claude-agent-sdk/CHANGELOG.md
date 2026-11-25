# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-11-25

Initial release: a Rust port of Anthropic's Claude Agent SDK (which ships for
Python and TypeScript). It drives the `claude` Claude Code CLI over the same
stream-JSON control protocol as the official SDKs.

### Added

- `query()` — one-shot, unidirectional streaming of typed messages for a prompt.
- `query_with_transport()` — the same, over a caller-supplied `Transport`.
- `ClaudeSDKClient` — bidirectional, stateful sessions with follow-ups,
  `interrupt`, `set_permission_mode`, `set_model`, MCP status/toggle/reconnect,
  file rewind, and task stop.
- `ClaudeAgentOptions` with a builder covering the Python SDK's option surface
  (model, tools, permissions, system prompt, MCP servers, agents, hooks,
  thinking/effort, sandbox, skills, plugins, structured output, and more).
- In-process MCP servers via `create_sdk_mcp_server()` and `tool()`, served to
  the CLI over the control channel (`initialize` / `tools/list` / `tools/call`
  / `ping`), with optional JSON-Schema argument validation (`validate` feature,
  on by default).
- `can_use_tool` permission callbacks and typed `hooks` (PreToolUse,
  PostToolUse, and the rest), each answered over the control protocol.
- Typed messages and content blocks (`Message`, `ContentBlock`, `ResultMessage`,
  `StreamEvent`, rate-limit and conversation-reset events), forward-compatible
  with unknown frame types.
- `SubprocessTransport`: CLI discovery, argument building, environment setup,
  NDJSON line framing with a buffer cap, a stderr callback, and graceful
  shutdown (terminate then kill).
- A typed error enum mirroring the Python SDK's exception hierarchy, including a
  `ResultError` carrying the CLI's terminal error payload.

### Fixed

- **`can_use_tool` now actually fires against the real CLI.** The callback
  requires the CLI to route permission prompts over the control channel
  (`--permission-prompt-tool stdio`); the first cut never set that, so against
  a real `claude` the callback was never consulted (mock tests masked it). Both
  `query()` and the client now configure this, reject a conflicting explicit
  `permission_prompt_tool_name`, and warn when `allowed_tools` or
  `bypassPermissions` would shadow the callback — mirroring the Python SDK.
- **`on_failure: Block` fails closed.** Connecting with a block-on-failure hook
  to a CLI that doesn't advertise `hook_on_failure_supported` (or reports
  `hooks_applied: false`) is refused instead of silently running fail-open.
- **No panic when a stream or client is dropped outside a Tokio runtime.**
  `Drop` previously called `tokio::spawn` unconditionally.
- **`control_cancel_request` aborts the in-flight handler** instead of being
  ignored, so an abandoned permission/hook/MCP request does no wasted work and
  writes no stale response; `close` aborts them all.
- **The `user` option is honored** (numeric uid or user name, Unix) rather than
  silently ignored.

### Added

- **Process failures carry the CLI's stderr.** stderr is always captured into a
  bounded tail (last 64 lines) and attached to `Error::Process`, replacing the
  old "check stderr output" placeholder — a failed spawn is diagnosable without
  configuring a callback.
- `ClaudeSDKClient::server_info()` / `Query::server_info()`: the `initialize`
  response (commands, output styles, models, account), previously discarded.
- `QueryStream::next_message` / `collect_messages` / `final_result`: consume a
  one-shot query without importing `futures::StreamExt`.
- `ClaudeAgentOptionsBuilder::hook(event, matcher)` for incremental hook setup.
- Every message and content-block type implements `Serialize`, so messages can
  be forwarded as JSON (e.g. streamed over HTTP). `Message` serializes with a
  `type` tag using the CLI's names; unknown frames and blocks pass through as-is.

### Performance

The message read path was rebuilt from first principles after profiling the
framing and parse pipeline (see `benches/throughput.rs`, `cargo bench`):

- **Line framing is now O(total bytes), not O(n²).** The first cut scanned the
  whole pending buffer from the front for each newline and `drain`ed every line
  off (memmoving the remainder). Framing now keeps a `start`/`scan` cursor,
  scans only newly read bytes with `memchr` (runtime SIMD), and compacts the
  buffer at most once per drained read. Measured throughput on 64 KiB-per-line
  streams rose from ~0.5 to ~4.4 GiB/s (~8x); a single 8 MiB message split
  across 16 KiB reads went from ~9 to ~108 MiB/s (~12x).
- **Frames parse straight from the buffer bytes.** `serde_json::from_slice` on
  the line replaces a `from_utf8_lossy` + `String` + `from_str` per line.
- **Typed messages move, not clone.** `parse_message` now consumes the `Value`
  and moves fields out instead of cloning each one, so a large payload is not
  duplicated into the typed struct (~9% faster on large messages, lower peak
  memory).
- **No lock on the message hot path.** Run-end bookkeeping moved from an async
  mutex taken on every forwarded frame to a std mutex taken only on `result` and
  `system` frames; `assistant` / `stream_event` / `user` frames take no lock.
- Dropped the redundant `BufReader` layer over the pipe (the framer buffers),
  removing one copy per read.
- **Spawning no longer copies the whole environment.** The transport inherits
  the parent environment and sets only the handful of SDK variables (removing
  `CLAUDECODE`), instead of cloning every inherited variable into a fresh map on
  each spawn — a real saving for a process that starts many sessions.
- **The framing buffer is released after a huge message.** A multi-megabyte
  tool result no longer pins that capacity for the rest of a long-lived session;
  the buffer shrinks back once drained.
- The control-request/response `pending` map moved from an async mutex to a std
  mutex (tiny synchronous critical sections).
- **Control frames are decoupled from data backpressure.** Message frames and
  control frames (`can_use_tool` / hook / MCP requests, and the answers to our
  own `interrupt` / `set_model`) used to share one small bounded queue, so a
  consumer that fell a few hundred messages behind could stall a control
  response the CLI was blocked waiting on — a head-of-line hazard shared with
  the Python and TypeScript SDKs. Data frames are now charged against a 64 MiB
  **byte** budget; control frames bypass it entirely. A slow consumer applies
  backpressure only after that much *data* is unconsumed, and never delays a
  control answer before then (`tests/protocol.rs::control_answered_while_consumer_stalls`).
  Budgeting by bytes rather than frame count is what keeps memory predictable
  across 40 B stream events and multi-MB tool results.
- **The transport reads lazily; no reader task, no intermediate channel.** The
  subprocess transport's stream now reads the pipe, frames and decodes as the
  engine polls it, instead of an eager task feeding a bounded queue — one task
  and one channel hop fewer per frame.
- **MCP tools compile their JSON Schema once.** `tools/call` used to compile the
  tool's schema on every invocation before validating; the compiled validator is
  now built with the tool. The `tools/list` payload is likewise built once.
- **CLI discovery is cached process-wide** (successes only), so a server that
  spawns many sessions doesn't rescan `PATH` and stat candidates each time.
- `SdkMcpServer::handle_jsonrpc` is public, so a server can be exercised directly
  in tests or served over a custom transport.
