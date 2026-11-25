#!/usr/bin/env python3
"""A tiny stand-in for the `claude` CLI, speaking just enough of the
stream-json control protocol to drive the Rust SDK's subprocess transport in
tests. It does NOT call any API.

Protocol it implements:
  * emits a `system`/`init` frame on startup;
  * answers the SDK's `initialize` control request with a success response;
  * for each `user` message on stdin, emits an `assistant` text turn echoing
    the prompt, then a `result` frame;
  * exits 0 when stdin closes.

It also honors an env var CLAUDE_FAKE_EXIT_NONZERO=1 to exit non-zero after the
first result (to test error propagation), emitting an error result first, and
a prompt starting with "HANG" makes it echo the assistant turn and then never
send a result (to test timeouts and cancellation). `set_model` control requests
change the model reported on later assistant turns.
"""
import json
import os
import sys


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def main():
    # Prove the SDK passes our env through and sets the entrypoint.
    entrypoint = os.environ.get("CLAUDE_CODE_ENTRYPOINT", "")
    default_model = os.environ.get("CLAUDE_FAKE_MODEL", "claude-fake")
    # Honors `set_model` control requests, like the real CLI, and reports the
    # current model on every assistant turn.
    current_model = default_model
    emit({
        "type": "system",
        "subtype": "init",
        "session_id": "fake-session",
        "uuid": "init-uuid",
        "model": default_model,
        "cwd": os.getcwd(),
        "tools": [],
        "mcp_servers": [],
        "permissionMode": "default",
        "slash_commands": [],
        "output_style": "default",
        "skills": [],
        "plugins": [],
        "apiKeySource": "none",
        "claude_code_version": "9.9.9",
        "entrypoint_seen": entrypoint,
        "claudecode_seen": os.environ.get("CLAUDECODE", ""),
    })

    if os.environ.get("CLAUDE_FAKE_STDERR_CRASH") == "1":
        sys.stderr.write("warning: something minor\n")
        sys.stderr.write("fatal: simulated crash in fake cli\n")
        sys.stderr.flush()
        sys.exit(2)

    turns = 0
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue

        if msg.get("type") == "control_request":
            req = msg.get("request", {})
            if req.get("subtype") == "initialize":
                emit({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": msg.get("request_id"),
                        "response": {"commands": [], "agents": []},
                    },
                })
            else:
                if req.get("subtype") == "set_model":
                    current_model = req.get("model") or default_model
                emit({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": msg.get("request_id"),
                        "response": {},
                    },
                })
            continue

        if msg.get("type") == "user":
            turns += 1
            content = msg.get("message", {}).get("content", "")
            if isinstance(content, list):
                content = " ".join(
                    b.get("text", "") for b in content if isinstance(b, dict)
                )
            emit({
                "type": "assistant",
                "message": {
                    "model": current_model,
                    "content": [{"type": "text", "text": f"echo: {content}"}],
                },
                "session_id": "fake-session",
                "uuid": f"assistant-{turns}",
            })
            if content.startswith("HANG"):
                # A stuck run: progress was made, but no result ever comes.
                continue
            if os.environ.get("CLAUDE_FAKE_EXIT_NONZERO") == "1":
                emit({
                    "type": "result",
                    "subtype": "error_during_execution",
                    "duration_ms": 1,
                    "duration_api_ms": 1,
                    "is_error": True,
                    "num_turns": turns,
                    "session_id": "fake-session",
                    "errors": ["simulated failure"],
                })
                sys.stdout.flush()
                sys.exit(3)
            emit({
                "type": "result",
                "subtype": "success",
                "duration_ms": 1,
                "duration_api_ms": 1,
                "is_error": False,
                "num_turns": turns,
                "session_id": "fake-session",
                "result": f"echo: {content}",
                "total_cost_usd": 0.0,
            })

    sys.exit(0)


if __name__ == "__main__":
    main()
