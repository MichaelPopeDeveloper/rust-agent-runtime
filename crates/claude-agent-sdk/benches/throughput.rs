//! Framing/parse throughput benchmark.
//!
//! Run with `cargo bench`. It compares the naive line reader (scan the whole
//! pending buffer from the front, then `drain` each line off — the shape the
//! SDK shipped first) against the strategy the SDK uses now (a `start`/`scan`
//! cursor with `memchr` and amortized compaction, parsing straight from the
//! buffer bytes). It also reports end-to-end parse throughput into typed
//! `Message`s.
//!
//! The point is the *shape* of the curves, not absolute numbers: the naive
//! reader is O(n²) in the size of a single message (every read rescans and
//! memmoves the growing buffer), so the gap widens sharply as messages get
//! large — which is exactly what big tool results and partial-message streams
//! produce.

use std::hint::black_box;
use std::time::Instant;

use serde_json::{json, Value};

/// Build an NDJSON byte stream of `count` messages, each whose text payload is
/// `payload_bytes` long (so a single line is large).
fn make_stream(count: usize, payload_bytes: usize) -> Vec<u8> {
    let filler = "x".repeat(payload_bytes);
    let mut out = Vec::new();
    for i in 0..count {
        let frame = json!({
            "type": "assistant",
            "message": {"model": "claude", "content": [{"type": "text", "text": format!("{i}:{filler}")}]},
            "session_id": "s",
            "uuid": format!("u{i}"),
        });
        out.extend_from_slice(serde_json::to_string(&frame).unwrap().as_bytes());
        out.push(b'\n');
    }
    out
}

/// The original shipped reader: `position` from the front + `drain(..=pos)`,
/// then `from_utf8_lossy` + `from_str`. Reproduced here so the benchmark can
/// show the before/after on one machine.
fn naive_count(stream: &[u8], chunk_size: usize) -> usize {
    let mut pending: Vec<u8> = Vec::new();
    let mut parsed = 0usize;
    for chunk in stream.chunks(chunk_size) {
        pending.extend_from_slice(chunk);
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            let text = String::from_utf8_lossy(line);
            let text = text.trim();
            if !text.is_empty() && text.starts_with('{') {
                let v: Value = serde_json::from_str(text).unwrap();
                parsed += black_box(&v).is_object() as usize;
            }
        }
    }
    parsed
}

/// The strategy the SDK uses now. This mirrors `framing::LineFramer` + parsing
/// straight from the line bytes. (The real type is crate-private, so the hot
/// logic is reproduced here for the comparison; `tests`/unit tests cover that
/// the real type behaves identically.)
fn optimized_count(stream: &[u8], chunk_size: usize) -> usize {
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut start = 0usize;
    let mut scan = 0usize;
    let mut parsed = 0usize;
    for chunk in stream.chunks(chunk_size) {
        buf.extend_from_slice(chunk);
        while let Some(rel) = memchr::memchr(b'\n', &buf[scan..]) {
            let nl = scan + rel;
            let line = &buf[start..nl];
            start = nl + 1;
            scan = start;
            let trimmed = trim(line);
            if !trimmed.is_empty() && trimmed[0] == b'{' {
                let v: Value = serde_json::from_slice(trimmed).unwrap();
                parsed += black_box(&v).is_object() as usize;
            }
        }
        if start == buf.len() {
            buf.clear();
            start = 0;
            scan = 0;
        } else if start >= 64 * 1024 {
            buf.drain(..start);
            scan -= start;
            start = 0;
        }
    }
    parsed
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [f, rest @ ..] = s {
        if f.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., l] = s {
        if l.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn bench(name: &str, bytes: usize, iters: usize, mut f: impl FnMut() -> usize) {
    // Warm up.
    black_box(f());
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        acc = acc.wrapping_add(f());
    }
    let elapsed = start.elapsed();
    black_box(acc);
    let total_bytes = (bytes * iters) as f64;
    let mib_per_s = total_bytes / elapsed.as_secs_f64() / (1024.0 * 1024.0);
    let per_iter_us = elapsed.as_secs_f64() * 1e6 / iters as f64;
    println!("  {name:<28} {per_iter_us:>10.1} us/iter   {mib_per_s:>9.1} MiB/s");
}

fn scenario(label: &str, count: usize, payload: usize, chunk: usize, iters: usize) {
    let stream = make_stream(count, payload);
    let bytes = stream.len();
    println!(
        "\n{label}  ({count} msgs x ~{payload}B payload = {:.2} MiB, {chunk}B reads)",
        bytes as f64 / (1024.0 * 1024.0)
    );
    bench("naive (position+drain)", bytes, iters, || {
        naive_count(&stream, chunk)
    });
    bench("optimized (cursor+memchr)", bytes, iters, || {
        optimized_count(&stream, chunk)
    });
}

/// Full pipeline: raw bytes -> typed `Message` via the SDK's public parser.
/// Measures the cost of the typed projection on top of framing + Value parse.
fn typed_pipeline(stream: &[u8], chunk_size: usize) -> usize {
    use claude_agent_sdk::parse_message;
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut start = 0usize;
    let mut scan = 0usize;
    let mut built = 0usize;
    for chunk in stream.chunks(chunk_size) {
        buf.extend_from_slice(chunk);
        while let Some(rel) = memchr::memchr(b'\n', &buf[scan..]) {
            let nl = scan + rel;
            let line = &buf[start..nl];
            start = nl + 1;
            scan = start;
            let trimmed = trim(line);
            if !trimmed.is_empty() && trimmed[0] == b'{' {
                let v: Value = serde_json::from_slice(trimmed).unwrap();
                let msg = parse_message(black_box(v)).unwrap();
                built += black_box(&msg).is_result() as usize + 1;
            }
        }
        if start == buf.len() {
            buf.clear();
            start = 0;
            scan = 0;
        } else if start >= 64 * 1024 {
            buf.drain(..start);
            scan -= start;
            start = 0;
        }
    }
    built
}

/// Time `iters` calls of `f`, reporting per-call latency and calls/second.
fn bench_ops(name: &str, iters: usize, mut f: impl FnMut() -> usize) {
    black_box(f());
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        acc = acc.wrapping_add(f());
    }
    let elapsed = start.elapsed();
    black_box(acc);
    let per_call_us = elapsed.as_secs_f64() * 1e6 / iters as f64;
    let per_s = iters as f64 / elapsed.as_secs_f64();
    println!("  {name:<34} {per_call_us:>9.2} us/call   {per_s:>11.0} calls/s");
}

/// In-process MCP `tools/call` throughput, and what a per-call JSON Schema
/// compile (the first implementation) was costing on top of it.
fn mcp_scenario() {
    use claude_agent_sdk::{create_sdk_mcp_server, tool, ToolResponse};

    let schema = json!({
        "type": "object",
        "properties": {
            "a": {"type": "number", "minimum": 0},
            "b": {"type": "number"},
            "op": {"type": "string", "enum": ["add", "mul", "sub"]},
            "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 10},
        },
        "required": ["a", "b", "op"],
        "additionalProperties": false,
    });
    let calc = tool("calc", "calc", schema.clone(), |args| async move {
        Ok(ToolResponse::text(format!(
            "{}",
            args["a"].as_f64().unwrap_or(0.0)
        )))
    });
    let server = create_sdk_mcp_server("calc", "1.0.0", vec![calc]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "calc", "arguments": {"a": 1, "b": 2, "op": "add", "tags": ["x"]}},
    });
    let args = call["params"]["arguments"].clone();

    println!("\nin-process MCP tools/call (schema-validated)");
    bench_ops("tools/call (validator cached)", 20_000, || {
        rt.block_on(server.handle_jsonrpc(&call)).is_some() as usize
    });
    bench_ops("schema compile + validate per call", 2_000, || {
        let validator = jsonschema::validator_for(&schema).unwrap();
        validator.validate(&args).is_ok() as usize
    });
}

fn main() {
    println!("framing + parse throughput (higher MiB/s is better)");
    // Many small messages (the typical chat/stream_event case).
    scenario("small messages", 5_000, 40, 16 * 1024, 50);
    // Fewer, large messages split across many reads (big tool results) —
    // this is where the naive reader's O(n²) per-message cost shows up.
    scenario("large messages", 200, 64 * 1024, 16 * 1024, 50);
    // One very large message across hundreds of reads (worst case for naive).
    scenario("one huge message", 1, 8 * 1024 * 1024, 16 * 1024, 20);

    // Typed projection: bytes -> Message. Shows the cost of building typed
    // structs on top of the Value parse (clone vs move out of the Value).
    println!("\ntyped projection: raw bytes -> Message");
    let small = make_stream(5_000, 40);
    bench("small -> Message", small.len(), 50, || {
        typed_pipeline(&small, 16 * 1024)
    });
    let large = make_stream(200, 64 * 1024);
    bench("large -> Message", large.len(), 50, || {
        typed_pipeline(&large, 16 * 1024)
    });

    mcp_scenario();
}
