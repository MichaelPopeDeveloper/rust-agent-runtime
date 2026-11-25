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
| POST | `/agent/run` | `{prompt, model?, system_prompt?, max_turns?, allowed_tools?, cwd?}` | `{session_id, result, is_error, subtype, num_turns, duration_ms, total_cost_usd, message_count}` |
| POST | `/agent/stream` | same as `/agent/run` | Server-Sent Events: one event per message (`assistant`, `result`, ...), then `done` |
| POST | `/messages` | a Messages API request body | the Messages API response, verbatim |
| POST | `/messages/complete` | `{prompt, model?, max_tokens?}` | `{"text": "..."}` |

Errors are JSON: `{"error": "..."}` with a meaningful status (400 bad request,
401 no/bad token, 403 disallowed `cwd`, 502 the agent or API failed, 503 the
server has no API key for `/messages`).

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

### Security posture

Running an agent with tools over HTTP is a sharp instrument. Defaults are
conservative: the server binds to loopback, clients cannot change the
permission mode or (unless enabled) the working directory, and nothing that
would prompt a human is approved non-interactively unless the operator allows
it via `AGENT_PERMISSION_MODE`/`AGENT_ALLOWED_TOOLS`. Set `AGENT_RUNTIME_TOKEN`
before exposing it beyond localhost, and put it behind TLS.

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
