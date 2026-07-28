# mcport

Fast, blocking, Tokio-free MCP stdio for Rust.

`mcport` is a small server runtime for applications that need MCP tools without
bringing an async executor into the process. It combines a compact builder API,
typed and raw tool handlers, a lower-level trait, zero-copy request routing,
canonical control-message recognition, and direct response serialization.

The `mcport` crate contains no Tokio, Hyper, Axum, futures executor, or unsafe
code.

## Install

```toml
[dependencies]
mcport = "0.1.0"
```

`mcport` uses `blazingly-json` for its public `Value`, `RawJson`, `RawValue`,
and `json!` surface. Applications can import those types from `mcport` and do
not need a second direct JSON dependency for MCP handling.

## Performance

Local Windows measurements on an Intel Core Ultra 7 255U. These are ranges
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

A typed builder handler is another 1.31-1.35x faster than the already-fast
`Value` handler for the measured tool call.

### Full-process competitor benchmark

The committed black-box harness also compares complete release binaries over
real stdin/stdout pipes. It uses `mcport 0.1.0`, the official Rust SDK
`rmcp 0.16.0`, and `rust-mcp-sdk 1.0.1`. Each server receives an initialize
handshake followed by 10,000 uniquely identified requests. Every complete run
warms each binary once, rotates server order across five measured rounds, and
reports the median.

These are ranges across three complete runs on the same Windows machine:

| Workload | Server | Median latency | Throughput | Versus mcport |
| --- | --- | ---: | ---: | ---: |
| tools/list | mcport | 4.41-5.67 us | 176,410-226,858 req/s | 1.00x |
| tools/list | rmcp 0.16.0 | 71.12-100.79 us | 9,922-14,060 req/s | 15.97-17.78x slower |
| tools/list | rust-mcp-sdk 1.0.1 | 107.23-139.74 us | 7,156-9,326 req/s | 24.33-25.43x slower |
| tools/call | mcport | 7.01-7.75 us | 129,047-142,698 req/s | 1.00x |
| tools/call | rmcp 0.16.0 | 61.84-96.21 us | 10,394-16,172 req/s | 8.82-12.71x slower |
| tools/call | rust-mcp-sdk 1.0.1 | 101.87-147.12 us | 6,797-9,816 req/s | 13.15-19.44x slower |

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
| mcport | 545,792 bytes |
| rmcp 0.16.0 | 1,633,280 bytes |
| rust-mcp-sdk 1.0.1 | 1,654,272 bytes |

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

`ToolServer::call_raw`, `identity_ref`, `catalog_ref`, and `catalog_raw_ref`
have compatible defaults. Existing implementations do not need to define
them; optimized implementations can override them.

## Embedding

- `serve(&mut server)` owns the standard blocking stdin/stdout loop;
- `serve_streams` accepts injectable `BufRead`/`Write` streams;
- `serve_message` processes one newline-free message and reports whether it
  wrote a response;
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

The fast recognizers accept only complete canonical layouts. They do not
partially trust lookalike input: reordered fields, whitespace variants, escaped
names, unusual request IDs, and other valid layouts fall back to the strict
order-independent `JsonCursor`; malformed inputs fall back far enough to produce
the same JSON-RPC semantics as `dispatch`.

`ToolReply::structured` accepts any `serde::Serialize` result. It serializes
once into a validated `RawValue`. Object roots are emitted both as compact text
and zero-copy `structuredContent`. Arrays, scalars, and null remain text-only
because the MCP schema requires `structuredContent` to be an object.

## Runtime behavior

- supports `initialize`, `ping`, `tools/list`, and `tools/call`;
- negotiates the current stable MCP revision `2025-11-25` and the legacy
  `2025-06-18` revision;
- strips UTF-8 BOMs from Windows-oriented input;
- consumes notifications without replies;
- returns JSON-RPC errors without terminating the stream;
- reports unknown tools as protocol errors and handler failures as MCP content
  errors with `isError: true`;
- mirrors object-shaped output into `structuredContent`; scalar and array
  results remain valid text-only results;
- defers application startup work until the first tool call if the catalog is
  cheap or prebuilt.

## Scope

The 0.1 runtime is deliberately server-only, stdio-only, and tools-only. It
does not yet implement resources, prompts, roots, sampling, completions,
subscriptions, tasks, progress, cancellation, OAuth, or remote HTTP
transports.

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

Replace the local path once the registry crate is available:

```toml
# before
mcport = { version = "0.1.0", path = "../mcport" }

# registry
mcport = "0.1.0"
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
cargo clippy --all-targets -- -D warnings
cargo +1.78 build --locked
cargo bench --bench runtime
cargo build --manifest-path benchmarks/competitors/Cargo.toml --release --bins
benchmarks/competitors/target/release/bench-runner
```

CI covers Linux, Windows, macOS, and Rust 1.78.

## Release

Version `0.1.0` is the first public release. It resolves `blazingly-json`
through crates.io, and the publish workflow repeats `cargo publish --dry-run`
before uploading the immutable crate.

## License

MIT
