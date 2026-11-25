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

## Performance

What was measured, how, and what changed. There are two layers: the SDK's
read path (CPU-bound, benchmarked with `cargo bench -p claude-agent-sdk`) and
the HTTP runtime's per-request lifecycle (dominated by process spawn and the
CLI's own API time, measured against the real `claude` CLI).

### Where the time goes

A trivial agent turn ("Reply only with: OK") costs 3.1–3.5 s *inside* the
CLI: the API round trip plus the CLI's own work. That part is the same for
every client of the CLI and is outside this code's control, and it varies with
API latency (the same prompt measured 3.1 s one minute and 6.2 s the next).
So the figure that matters, and the one the tables below use, is **overhead**:
the wall-clock time of the HTTP request minus the `duration_ms` the CLI
reports in its result frame. That is the part this code is responsible for.

### Runtime: per-request overhead against the real CLI

The first version of the runtime spawned a fresh `claude` process for every
request, waited for it to exit before answering, and had no limits on how
many ran at once or for how long.

| Path | Overhead before | Overhead now | What changed |
|---|---|---|---|
| One-shot `/agent/run`, cold | **2.75 s** (6.04 s wall for 3.29 s of CLI work) | **0.6–0.9 s** | The process exit moved off the response path: the response returns the moment the result arrives, teardown runs in the background while holding the slot. What remains is the spawn + `initialize` handshake. |
| One-shot `/agent/run`, warm spare | n/a (no spares) | **80–100 ms** | The spawn and handshake already happened in the background (`AGENT_WARM_SPARES`). |
| Follow-up turn on a kept session | n/a (a fresh process per request) | **10–15 ms** | Same process, same conversation (`keep_session` / `session_id`): no spawn, no handshake, and the model keeps its context. |
| Follow-up that switches model | n/a | ~0.8 s | The CLI's own model switch; later turns are back to 10–15 ms. |

Put differently: for a workload of short follow-up turns, the runtime went
from adding roughly 85% on top of the CLI's time to adding well under 1%.

Memory and process count:

| | Before | Now |
|---|---|---|
| RSS of one `claude` process during a run | 220–307 MB | the same (it is the CLI) |
| Processes under load | unbounded: one per in-flight request | at most `AGENT_MAX_CONCURRENT` running + `AGENT_MAX_SESSIONS` kept + `AGENT_WARM_SPARES` warm |
| A request past the limit | spawns anyway | `429` at once (or after `AGENT_QUEUE_WAIT_SECS`) |
| A stuck turn | holds its process forever | `504` at `AGENT_RUN_TIMEOUT_SECS`, process terminated |
| Server exit with live processes | orphaned `claude` processes | all closed, concurrently; measured ~1 s from SIGTERM to exit with two alive, none left |
| Kept session idle | n/a | reclaimed after `AGENT_SESSION_TTL_SECS` |

Smaller wins on the same path: a streaming consumer that disconnects frees
its slot immediately rather than at the next message; status polls stay out
of the request log; the direct `/messages` client retries `429`/`529`/`5xx`
with backoff and never hangs on a dead connection; the release binary is built
with LTO and stripped (9.3 MB).

### SDK: the read path

`benches/throughput.rs` feeds the framer and parser synthetic NDJSON in 16 KiB
reads, the way the pipe delivers it, and compares the first implementation
("naive": scan the whole pending buffer for each newline, `drain` every line)
with the shipped one ("optimized": a `start`/`scan` cursor, `memchr` over new
bytes only, one compaction per drained read). Numbers from this machine; run
it yourself for yours.

| Scenario | Naive | Optimized | Speedup |
|---|---|---|---|
| 200 messages of 64 KiB (12.5 MiB) | 741 MiB/s | 4304 MiB/s | 5.8x |
| One 8 MiB message | 12.3 MiB/s | 87.5 MiB/s | 7.1x |
| 5000 messages of ~40 B | 185 MiB/s | 197 MiB/s | ~1x (parse-bound; framing was never the cost here) |
| Raw bytes → typed `Message`, large | | 4124 MiB/s | moves fields out of the JSON instead of cloning (~9% over the clone version, lower peak memory) |
| Raw bytes → typed `Message`, small | | 111 MiB/s | |
| In-process MCP `tools/call`, schema-validated | 6.44 µs/call | 1.41 µs/call | 4.6x (validator and `tools/list` compiled once, 710k calls/s) |

The first row is the common case for tool-heavy agents (file reads, command
output); the second is a multi-megabyte tool result, where the old framer's
quadratic rescans dominated. Beyond throughput, the SDK's read path was
rebuilt around a few invariants worth knowing when you tune the runtime:

- **Control frames are never starved by data.** Permission prompts, hook
  callbacks and MCP calls bypass the data queue, so a consumer that falls
  behind cannot stall an answer the CLI is blocked on. Memory for unconsumed
  data is bounded by a 64 MiB **byte** budget rather than a frame count, which
  is what keeps it predictable across 40 B stream events and 8 MiB results.
- **No lock on the hot path.** Only `result` and `system` frames touch
  bookkeeping state; `assistant`, `user` and `stream_event` frames take no
  lock at all.
- **The transport reads lazily** as the engine polls it: no reader task and
  no intermediate channel hop per frame.
- **Spawn cost is kept to the process itself.** The child inherits the
  environment instead of receiving a copied map, and CLI discovery is cached
  process-wide, so a server that starts many sessions doesn't re-scan `PATH`.
- **Buffers shrink back** after a huge message instead of pinning that
  capacity for the life of a session.

### Reproducing the numbers

SDK read path (release build, prints MiB/s and µs/call):

```bash
cargo bench -p claude-agent-sdk
```

Runtime overhead, against your own CLI login. Start the server, then time a
cold run that keeps its session and a follow-up on it; subtract each
response's `duration_ms` from the wall time:

```bash
cargo run --release -p agent-runtime
```

```bash
time curl -s -X POST localhost:3000/agent/run -H 'content-type: application/json' \
  -d '{"prompt": "Reply only with: OK", "keep_session": true}'
```

```bash
time curl -s -X POST localhost:3000/agent/run -H 'content-type: application/json' \
  -d '{"prompt": "Reply only with: OK", "session_id": "<session_id from above>"}'
```

`GET /agent/sessions` shows warm spares, running turns and the counters
(warm hits vs. spawns, timeouts, `429`s) that tell you whether the limits fit
your traffic. Note that `total_cost_usd` in a kept session is cumulative, so
compare per-turn cost by difference.

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
