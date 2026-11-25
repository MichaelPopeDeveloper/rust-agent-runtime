# rust-agent-runtime

A Rust agent runtime for Claude, in two crates:

| Crate | What it is |
|---|---|
| [`crates/claude-agent-sdk`](crates/claude-agent-sdk) | A Rust port of Anthropic's Claude Agent SDK (Python/TypeScript). Drives the `claude` Claude Code CLI over its control protocol: built-in tools, MCP servers, subagents, hooks, permissions, sessions. The engine. |
| [`crates/agent-runtime`](crates/agent-runtime) | An HTTP service (`axum`) that runs agents through the SDK and also fronts the Anthropic Messages API directly. The thing you deploy. |

The runtime grew out of the original scaffold in this repo — a single `reqwest`
call to the Messages API with `.env` config and an `axum` dependency. That path
is kept (and made correct) as the `/messages` routes; the agent routes add the
full Claude Code harness via the SDK.

## Run the service

Requirements: Rust 1.80+, and for agent runs the `claude` CLI installed and
logged in (`npm install -g @anthropic-ai/claude-code`). The direct Messages
routes need `ANTHROPIC_API_KEY` instead.

```bash
cp .env.example .env   # optional; or export the variables
cargo run -p agent-runtime
```

```bash
curl -s localhost:3000/health
curl -s -X POST localhost:3000/agent/run \
  -H 'content-type: application/json' \
  -d '{"prompt": "Summarize the README in this directory.", "allowed_tools": ["Read", "Glob"]}'
```

## Endpoints

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/health` | — | `{"status":"ok"}` (never needs auth) |
| POST | `/agent/run` | `{prompt, model?, system_prompt?, max_turns?, allowed_tools?, cwd?, session_id?, keep_session?}` | `{session_id, result, is_error, subtype, num_turns, duration_ms, total_cost_usd, model, message_count, session_reused, session_kept}` |
| POST | `/agent/stream` | same as `/agent/run` | Server-Sent Events: one event per message (`assistant`, `result`, ...), `error` on failure, then always `done` with `{session_id, session_kept}` |
| GET | `/agent/sessions` | — | `{count, max_sessions, running, max_concurrent_runs, warm_spares: {ready, target}, stats: {...}, sessions: [{id, idle_secs}]}` |
| DELETE | `/agent/sessions/{id}` | — | `204`; the session's process exits |
| POST | `/messages` | a Messages API request body | the Messages API response, verbatim; with `"stream": true` the event stream is relayed as it arrives |
| POST | `/messages/complete` | `{prompt, model?, max_tokens?}` | `{"text": "..."}` |

Errors are JSON: `{"error": "..."}` with a meaningful status (also for a
malformed body or an unknown route): 400 bad request,
401 no/bad token, 403 disallowed `cwd`, 404 unknown/expired session, 409 the
session is busy with another turn, 429 too many concurrent runs (retry later),
502 the agent or API failed, 503 the server has no API key for `/messages`,
504 the turn exceeded the run timeout (its process was terminated).

### Sessions: keep the process between turns

Every fresh agent run spawns the `claude` CLI, which costs a couple of seconds
and a few hundred MB before the first token, and starts a new conversation. To
avoid both, keep the session:

```bash
# First turn: keep the process alive afterwards.
curl -s -X POST localhost:3000/agent/run -H 'content-type: application/json' \
  -d '{"prompt": "Read src/main.rs and remember its structure.", "allowed_tools": ["Read"], "keep_session": true}'
# -> {"session_id": "6c1e...", "session_kept": true, ...}

# Follow-up: same process, same conversation, no spawn.
curl -s -X POST localhost:3000/agent/run -H 'content-type: application/json' \
  -d '{"prompt": "Now summarize what main() does.", "session_id": "6c1e..."}'
# -> {"session_reused": true, "session_kept": true, ...}

# Done with it (or let it expire after AGENT_SESSION_TTL_SECS idle).
curl -s -X DELETE localhost:3000/agent/sessions/6c1e...
```

`keep_session` defaults to `true` when `session_id` is given and `false`
otherwise, so one-shot calls stay one-shot. Note that `total_cost_usd` is the
CLI's figure, which is cumulative over the session, not per turn. A session's
tools, `cwd`, system prompt and turn cap are fixed when it starts; a follow-up
may pass `model` to switch the session's model (it sticks for later turns),
and the response's `model` says which model answered. A session runs one turn
at a time (a concurrent turn gets `409`), and a turn that times out or errors
evicts its session, since its state can't be trusted.

### Capacity

Agent runs are bounded by the server, not by whoever sends requests:

- **Concurrency**: at most `AGENT_MAX_CONCURRENT` turns run at once. Beyond
  that a request gets `429` immediately rather than queueing, so load is
  visible to the caller and memory stays bounded (each run is a `claude`
  process of roughly 200–300 MB). Set `AGENT_QUEUE_WAIT_SECS` to let requests
  wait that long for a slot first, for bursty callers that would rather wait
  than retry.
- **Warm spares**: a fresh run normally pays 600–800 ms to spawn the CLI and
  complete its handshake before the first token. The server keeps
  `AGENT_WARM_SPARES` processes pre-spawned with the default options, so a
  request without per-request overrides (`model`, `system_prompt`,
  `max_turns`, `allowed_tools`, `cwd`) starts at once; the pool refills in the
  background. Warming begins with the first agent request, so a deployment
  that only uses `/messages` never pays for it. Total `claude` processes are
  bounded by `AGENT_MAX_CONCURRENT + AGENT_MAX_SESSIONS + AGENT_WARM_SPARES`.
- **Run timeout**: a turn is cut off at `AGENT_RUN_TIMEOUT_SECS` with `504`
  and its process terminated, so a stuck run can't hold a slot forever.
- **Kept sessions**: at most `AGENT_MAX_SESSIONS` live, reclaimed after
  `AGENT_SESSION_TTL_SECS` idle. `GET /agent/sessions` shows the live set and
  the current load.
- **Shutdown**: on SIGINT/SIGTERM the server stops accepting, lets in-flight
  turns finish, then closes every kept session and warm spare, so no `claude`
  process is orphaned. Each phase is bounded by `AGENT_SHUTDOWN_GRACE_SECS`;
  anything still running after that is dropped and its process killed.
- **Visibility**: every request is logged with method, path, status and
  latency, and `GET /agent/sessions` reports live sessions, warm spares,
  running turns and counters since startup (completed, failed, timed out,
  `429`s, `409`s, warm hits vs. spawns) for tuning the limits above.

## Configuration

All via environment variables (a `.env` file is loaded if present):

| Variable | Default | Meaning |
|---|---|---|
| `BIND` / `PORT` | `127.0.0.1` / `3000` | Listen address. Loopback by default on purpose. |
| `AGENT_RUNTIME_TOKEN` | unset | If set, every route but `/health` requires `Authorization: Bearer <token>`. |
| `ANTHROPIC_API_KEY` | unset | Key for the `/messages` routes only. Agent runs use the CLI's own login. |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Messages API base URL. |
| `CLAUDE_CLI_PATH` | discovered on `PATH` | Explicit `claude` executable. |
| `AGENT_PERMISSION_MODE` | CLI default | Permission mode for agent runs (`acceptEdits`, `plan`, ...). Clients can't change it. |
| `AGENT_ALLOWED_TOOLS` | none | Default tools allowed without prompting (comma-separated). |
| `AGENT_RUNTIME_ALLOW_CWD` | off | Let clients set the agent's working directory. |
| `AGENT_MODEL` | CLI default | Default model for runs and completions. |
| `AGENT_MAX_CONCURRENT` | `4` | Agent turns that may run at once; more get `429`. |
| `AGENT_RUN_TIMEOUT_SECS` | `600` | Wall-clock limit per turn; longer gets `504` and the process is terminated. |
| `AGENT_SESSION_TTL_SECS` | `300` | Idle time after which a kept session's process is reclaimed. |
| `AGENT_MAX_SESSIONS` | `32` | Most kept sessions (idle-but-live processes) at once. |
| `AGENT_WARM_SPARES` | `1` | Pre-spawned processes for fresh runs with default options; `0` disables. |
| `AGENT_QUEUE_WAIT_SECS` | `0` | How long a request waits for a free slot before `429`. |
| `AGENT_SHUTDOWN_GRACE_SECS` | `30` | Time in-flight turns and process teardowns get after a shutdown signal. |

### Security posture

Running an agent with tools over HTTP is a sharp instrument. Defaults are
conservative: the server binds to loopback, clients cannot change the
permission mode or (unless enabled) the working directory, and nothing that
would prompt a human is approved non-interactively unless the operator allows
it via `AGENT_PERMISSION_MODE`/`AGENT_ALLOWED_TOOLS`. Set `AGENT_RUNTIME_TOKEN`
before exposing it beyond localhost, and put it behind TLS. The token check is
constant-time, and the concurrency and timeout limits above mean a flood of
requests degrades into `429`s rather than into an unbounded number of
processes.

The direct `/messages` routes retry transient upstream failures (`429`, `529`
overloaded, other `5xx`, connection errors) twice with backoff, honouring
`retry-after`, and time out rather than hanging on a dead connection.

## Use the SDK directly

If you want the agent engine in your own Rust program rather than over HTTP,
depend on the SDK crate; see [its README](crates/claude-agent-sdk/README.md)
and `examples/`.

```rust,no_run
use claude_agent_sdk::{query, ClaudeAgentOptions};

# async fn run() -> claude_agent_sdk::Result<()> {
let result = query("What is 2 + 2?", ClaudeAgentOptions::default())
    .await?
    .final_result()
    .await?;
println!("{}", result.and_then(|r| r.result).unwrap_or_default());
# Ok(())
# }
```

## Development

```bash
cargo build --workspace
cargo test --workspace          # SDK unit/protocol/subprocess tests + runtime HTTP tests
cargo clippy --workspace --all-targets -- -D warnings
cargo bench -p claude-agent-sdk # framing/parse/MCP throughput
```

The tests need no network or API key: agent paths run against a fake `claude`
fixture, and the Messages client against a mock upstream.

## License

MIT.
