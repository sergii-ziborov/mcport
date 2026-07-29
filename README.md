# mcport

Fast, bounded, Tokio-free MCP stdio for Rust.

`mcport` is a small server runtime for applications that need MCP tools without
bringing an async executor into the process. It combines a compact builder API,
typed and raw tool handlers, a lower-level trait, zero-copy request routing,
canonical control-message recognition, direct response serialization, and an
optional controlled scheduler for expensive handlers.

The `mcport` crate contains no Tokio, Hyper, Axum, futures executor, or unsafe
code.

## Install

Published stable release:

```toml
[dependencies]
mcport = "0.3.0"
```

Or run `cargo add mcport@0.3.0`.

`mcport` uses `blazingly-json` for its public `Value`, `RawJson`, `RawValue`,
and `json!` surface. Applications can import those types from `mcport` and do
not need a second direct JSON dependency for MCP handling.

## Performance

The table below records the published `0.1.0` baseline on an Intel Core Ultra 7
255U running Windows. These are ranges
across three complete release-profile runs; each run alternates implementation
order for 24 rounds and reports the median. Both paths use the same
`blazingly-json` engine, so the difference measures runtime architecture rather
than a parser swap. Absolute latency varied with system load, while the
relative advantage remained stable.

| Complete request/response | Direct mcport | Original owned-DOM path | Speedup |
| --- | ---: | ---: | ---: |
| ping | 23.55-27.55 ns | 707.21-863.02 ns | 29.65-34.41x |
| initialize | 402.84-531.14 ns | 4,565.21-5,119.87 ns | 9.19-12.49x |
| tools/list | 2,128.22-2,736.24 ns | 7,877.45-9,916.41 ns | 3.62-3.78x |
| tools/call | 2,150.33-2,332.38 ns | 8,409.90-8,739.66 ns | 3.66-3.97x |

A typed builder handler was another 1.31-1.35x faster than the already-fast
`Value` handler for the measured tool call.

The `0.3.0` work keeps `serve_message` as the canonical fast path. New modern
protocol fallbacks and pagination are placed on cold paths. The black-box
runner can also compare a current binary against an exact baseline binary in
alternating pairs and fail on a configured latency regression; see
`benchmarks/competitors/README.md`.

### Full-process competitor benchmark

The committed black-box harness also compares complete release binaries over
real stdin/stdout pipes. The table uses `mcport 0.3.0`, the official Rust SDK
`rmcp 0.16.0`, and `rust-mcp-sdk 1.0.1`. Each server receives an initialize
handshake followed by 10,000 uniquely identified requests. Every complete run
warms each binary once, rotates server order across five measured rounds, and
reports the median.

These are ranges across three complete runs on the same Windows machine:

| Workload | Server | Median latency | Throughput | Versus mcport |
| --- | --- | ---: | ---: | ---: |
| tools/list | mcport | 5.41-9.13 us | 109,473-184,988 req/s | 1.00x |
| tools/list | rmcp 0.16.0 | 91.72-218.26 us | 4,582-10,903 req/s | 16.97-23.89x slower |
| tools/list | rust-mcp-sdk 1.0.1 | 133.98-253.22 us | 3,949-7,464 req/s | 24.78-28.31x slower |
| tools/call | mcport | 7.39-9.88 us | 101,233-135,296 req/s | 1.00x |
| tools/call | rmcp 0.16.0 | 82.31-112.96 us | 8,853-12,149 req/s | 8.92-11.44x slower |
| tools/call | rust-mcp-sdk 1.0.1 | 121.31-152.15 us | 6,573-8,243 req/s | 13.15-16.42x slower |

The harness validates the final response from every warmup: `tools/list` must
contain `query_graph`, while `tools/call` must reproduce all four structured
fields. It also rejects any protocol error and requires exactly 10,001
responses per process. `tools/list` emits the same 288.9 average bytes per
response in all three implementations. For `tools/call`, mcport and rmcp emit
259.9 bytes while rust-mcp-sdk emits 243.9 bytes; mcport is not winning by
omitting the explicit `isError: false` field.

With identical thin-LTO release settings, the benchmark binaries are:

| Binary | Size |
| --- | ---: |
| mcport | 411,136 bytes |
| rmcp 0.16.0 | 1,440,256 bytes |
| rust-mcp-sdk 1.0.1 | 1,469,440 bytes |

The competitor SDKs intentionally live only in
`benchmarks/competitors`, which the published crate excludes. Their Tokio and
`serde_json` dependencies never enter mcport's normal dependency graph.

Run the black-box harness:

```text
cargo build --manifest-path benchmarks/competitors/Cargo.toml --release --bins
benchmarks/competitors/target/release/bench-runner
```

This is a selected local tools-only stdio workload, not a claim that mcport
implements the broader feature sets of either SDK. The benchmark includes
process startup, OS pipes, each runtime and codec, dispatch, handler work, and
complete response output.

### What the benchmark measures

`benches/runtime.rs` exercises a complete in-memory request/response cycle:

1. accept one UTF-8 JSON-RPC message;
2. recognize or parse the route;
3. negotiate/list/dispatch as appropriate;
4. deserialize tool arguments;
5. execute the same deterministic handler;
6. construct and serialize the complete MCP response plus newline.

The `direct` side calls `serve_message`. The comparison side first parses the
whole request into an owned `Value`, calls the public `dispatch` compatibility
API, then serializes that owned response. Before timing begins, the harness
parses both responses and asserts semantic equality.

Each row uses 25,000 operations per round and 24 alternating rounds. The table
reports ranges of the per-run medians from three complete runs. Output is a
preallocated `Vec<u8>`, so the numbers isolate runtime/codec cost; they do not
claim filesystem, process scheduling, pipe, client, or actual tool-work
latency. Run the benchmark on the deployment machine before using absolute
nanoseconds for capacity planning.

Why the direct path wins:

- compact `ping`, `initialize`, `tools/list`, and `tools/call` share one exact
  zero-allocation recognizer that parses the common prefix and request id once;
- reordered, spaced, or escaped input falls back to the strict
  order-independent `JsonCursor`;
- routing fields and raw tool arguments borrow from the input line;
- typed/raw handlers skip an intermediate mutable JSON DOM;
- `ToolReply::structured` serializes an arbitrary Serde result once into a
  `RawValue`, then reuses the same bytes for text and `structuredContent`;
- canonical response envelopes are written directly;
- the builder lends its identity and validated pre-serialized catalog instead
  of cloning or repeatedly serializing them;
- the stdio loop reuses one input buffer for the full session;
- structurally unusual but valid legacy requests fall back to the public
  owned dispatcher, preserving response semantics.

Run the committed harness:

```text
cargo bench --bench runtime
```

For a stable comparison, use a release build, close CPU-heavy work, run the
harness several times, and compare the ratio as well as absolute latency.

## Minimal server

```rust
use mcport::{json, McpServer, ToolReply};

fn main() -> std::io::Result<()> {
    let mut server = McpServer::new("echo", "1.0.0")
        .instructions("Echoes tool arguments back.")
        .tool(
            "echo",
            "Echo the arguments.",
            json!({
                "type": "object",
                "additionalProperties": true
            }),
            ToolReply::structured,
        );

    server.serve()
}
```

`McpServer` is the smallest mode: one request is dispatched inline at a time.
Its stdio adapter still enforces bounded input and atomic bounded output, with
8 MiB request and response defaults. Use `serve_with_limits` to choose lower
budgets. Oversized input is drained through the next newline; oversized output
is replaced by one valid JSON-RPC error rather than truncated bytes.
`FlushPolicy::PerMessage` is the interactive default. Throughput-oriented
adapters may opt into `FlushPolicy::Batch { max_messages }` through
`TransportConfig` and `serve_with_config`; a partial final batch is always
flushed at EOF. Batching deliberately trades response latency for fewer flush
boundaries.

## Controlled runtime

Use `ConcurrentMcpServer` when tools can be slow, parallel, cancellable, or
untrusted enough to require scheduling policy:

```rust
use mcport::{
    json, ConcurrentMcpServer, FlushPolicy, RuntimeConfig, ToolReply,
    TransportLimits,
};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let config = RuntimeConfig {
        transport: TransportLimits::new(1024 * 1024, 1024 * 1024),
        max_in_flight: 4,
        queue_depth: 32,
        output_queue_depth: 32,
        output_flush_policy: FlushPolicy::PerMessage,
        handler_deadline: Some(Duration::from_secs(10)),
    };

    ConcurrentMcpServer::new("worker", "1.0.0")
        .tool(
            "work",
            "Runs bounded cooperative work.",
            json!({"type": "object"}),
            |context, arguments| {
                if context.is_cancelled() {
                    return ToolReply::error("cancelled");
                }
                let _ = context.report_progress(1.0, Some(1.0), Some("done"));
                ToolReply::structured(arguments)
            },
        )
        .serve(config)
}
```

This mode provides a fixed handler-slot ceiling, bounded request and output
queues, deadlines, per-request cancellation tokens, panic-to-protocol-error
isolation, and a bounded progress channel. Client cancellation is cooperative:
handlers must observe `RequestContext::is_cancelled` or its token. A handler
that ignores cancellation cannot be forcibly killed safely by a Rust thread;
its slot remains occupied until it exits, preventing detached runaway work
from creating unbounded concurrency. Hard termination belongs in a separate
process or another isolation boundary. Panic isolation applies when the binary
uses unwinding; `panic = "abort"` still terminates the process.

## Typed tool

Typed handlers deserialize their arguments directly from validated raw JSON.
They do not construct an intermediate `Value`.

```rust
use mcport::{json, McpServer, ToolReply};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Add {
    left: i64,
    right: i64,
}

#[derive(Serialize)]
struct Sum {
    value: i64,
}

let server = McpServer::new("math", "1.0.0").typed_tool(
    "add",
    "Adds two integers.",
    json!({
        "type": "object",
        "properties": {
            "left": {"type": "integer"},
            "right": {"type": "integer"}
        },
        "required": ["left", "right"]
    }),
    |arguments: Add| {
        ToolReply::structured(Sum {
            value: arguments.left + arguments.right,
        })
    },
);
# let _ = server;
```

Use `raw_tool` when a handler already has its own codec or wants exact access
to the validated argument JSON.

## Shared state

```rust
use mcport::{json, McpServer, ToolReply};
use serde::Deserialize;

#[derive(Deserialize)]
struct Increment {
    amount: u64,
}

let server = McpServer::with_state("counter", "1.0.0", 0_u64)
    .typed_tool_with_state(
        "increment",
        "Increments the counter.",
        json!({"type": "object"}),
        |counter, arguments: Increment| {
            *counter += arguments.amount;
            ToolReply::structured(json!({"count": *counter}))
        },
    );
# let _ = server;
```

## Custom static dispatch

The original three-method trait remains available for applications that prefer
compile-time dispatch or generate their own catalog.

```rust
use mcport::{json, ServerIdentity, ToolReply, ToolServer, Value};

struct Echo;

impl ToolServer for Echo {
    fn identity(&self) -> ServerIdentity {
        ServerIdentity::new("echo", "1.0.0", "Echoes arguments.")
    }

    fn catalog(&mut self) -> Value {
        json!([{
            "name": "echo",
            "description": "Echo.",
            "inputSchema": {"type": "object"}
        }])
    }

    fn call(&mut self, name: &str, arguments: Value) -> ToolReply {
        match name {
            "echo" => ToolReply::structured(arguments),
            _ => ToolReply::error(format!("unknown tool: {name}")),
        }
    }
}
```

`ToolServer::call_raw`, identity/catalog borrowing, and pagination methods have
compatible defaults. Existing implementations do not need to define them;
optimized implementations can override them. Both builders expose
`tool_page_size` for opaque cursor pagination.

## Embedding

- `serve(&mut server)` owns the bounded blocking stdin/stdout loop;
- `serve_with_limits` sets explicit request and response byte budgets;
- `serve_with_config` additionally selects per-message or batched flushing;
- `serve_streams` and `serve_streams_with_limits` accept injectable streams;
- `serve_message` processes one newline-free message and reports whether it
  wrote a response; framing and byte budgets remain the embedding transport's
  responsibility;
- `serve_controlled` and `serve_controlled_streams` run a thread-safe
  `ConcurrentToolServer` under `RuntimeConfig`;
- `dispatch` retains the owned `Value` API for compatibility and testing.

## Protocol contract

| Input | Result |
| --- | --- |
| valid request with `id` | one newline-terminated JSON-RPC response |
| valid notification without `id` | no response |
| malformed JSON | `-32700` parse error |
| malformed JSON-RPC request/version | `-32600` invalid request |
| unsupported method | `-32601` method not found |
| missing/unknown tool | `-32602` invalid params |
| registered handler failure | successful JSON-RPC envelope with MCP `isError: true` |
| request over `max_request_bytes` | bounded `-32000` error; frame is drained |
| response over `max_response_bytes` | atomic `-32000` error, never partial JSON |

The fast recognizers accept only complete canonical layouts. They do not
partially trust lookalike input: reordered fields, whitespace variants, escaped
names, unusual request IDs, and other valid layouts fall back to the strict
order-independent `JsonCursor`; malformed inputs fall back far enough to produce
the same JSON-RPC semantics as `dispatch`.

`ToolReply::structured` accepts any `serde::Serialize` result. It serializes
once into a validated `RawValue`. Object roots are emitted both as compact text
and zero-copy `structuredContent` in every supported revision. MCP 2026-07-28
also permits arrays, scalars, and null as `structuredContent`; legacy responses
keep those values text-only.

## Runtime behavior

- supports legacy `initialize`, `ping`, `tools/list`, and `tools/call`;
- supports stateless MCP `2026-07-28` `server/discover` and per-request protocol
  metadata while retaining `2025-11-25` and `2025-06-18` compatibility;
- strips UTF-8 BOMs from Windows-oriented input;
- rejects invalid UTF-8, incomplete EOF frames, and oversized lines without
  unbounded line allocation;
- consumes notifications without replies;
- returns JSON-RPC errors without terminating the stream;
- supports opaque cursor pagination for `tools/list`;
- lets servers compose initialization/discovery capabilities and handle
  runtime-neutral JSON-RPC extensions such as `resources/list`,
  `resources/templates/list`, and `resources/read`;
- reports unknown tools as protocol errors and handler failures as MCP content
  errors with `isError: true`;
- mirrors object-shaped output into `structuredContent`; scalar and array
  results remain valid text-only results;
- defers application startup work until the first tool call if the catalog is
  cheap or prebuilt.

## Scope

The `0.3.0` runtime remains server-only and stdio-only. It covers
bounded framing/output, controlled concurrency, queue backpressure, handler
deadlines, cooperative cancellation, progress, panic isolation, and tool
pagination. The core owns the tools protocol and exposes capability composition
plus a typed `MethodReply` extension hook for resources and other
runtime-neutral request families. It does not prescribe resource storage or
implement prompts, roots, sampling, completions, subscriptions, tasks, OAuth,
or remote HTTP transports.

Those capabilities should grow as runtime-neutral protocol layers with
separate transport adapters. The blocking stdio default must remain Tokio-free;
an optional remote adapter must not pull an executor into the core.

This makes `mcport` narrower than the official Rust MCP SDK and
`rust-mcp-sdk`, but substantially smaller and faster for its selected local
tool-server workload.

## Dependencies and safety

Direct runtime dependencies:

- `blazingly-json` for JSON, borrowed routing, and canonical recognition;
- `serde` for typed handlers and direct response serialization.

`unsafe_code` is forbidden in `mcport`. Its `blazingly-json` dependency keeps
its small audited unsafe allowance isolated to `raw_value.rs`. Tokio, Hyper,
Axum, and `serde_json` are absent from the normal dependency tree.

The two direct runtime dependencies are:

- `blazingly-json = 0.1.0`;
- `serde`, with derive support for typed handlers and response structs.

There are no default-feature switches that silently add a network stack or
executor.

## Migrating an existing MCP server

The published registry dependency is:

```toml
mcport = "0.3.0"
```

For code still importing `serde_json` everywhere, migration can be staged by
aliasing only the package name first:

```toml
serde_json = { package = "blazingly-json", version = "0.1.0" }
```

That preserves existing `serde_json::Value`, `json!`, `from_*`, and `to_*`
paths while moving them to the new engine. A full `weavatrix-rust
--all-features` consumer probe with this alias and the released mcport source
passes 32 tests. This is compatibility evidence, not an automatic edit of the
consumer repository.

## Verification

```text
cargo fmt --all -- --check
cargo test --locked
cargo test --locked --features subprocess-tests --test stdio_subprocess
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo +1.78 build --locked
cargo bench --bench runtime
cargo build --manifest-path benchmarks/competitors/Cargo.toml --release --bins
benchmarks/competitors/target/release/bench-runner
```

The feature-gated subprocess suite exercises real pipes, fragmented input,
partial EOF, invalid UTF-8, oversized frames, slow readers, cancellation,
progress, panic isolation, response overflow, and repeated sessions. CI runs
it on Linux, Windows, and macOS in addition to the Rust 1.78 build.

An Inspector smoke test can target the fixture without adding Inspector to the
runtime dependency graph:

```text
cargo build --features subprocess-tests --bin mcport-test-server
npx -y @modelcontextprotocol/inspector@latest --cli \
  target/debug/mcport-test-server --method tools/list
```

## Release

Published: `mcport 0.3.0` is available from crates.io and can be installed with
`cargo add mcport@0.3.0`. Earlier releases remain available for applications
that only need the original inline server surface.

## License

MIT
