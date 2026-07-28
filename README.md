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
| ping | 84.43-120.88 ns | 1,073.72-1,693.27 ns | 12.72-14.01x |
| initialize | 592.39-1,034.92 ns | 6,292.58-10,608.02 ns | 10.25-13.99x |
| tools/list | 2,292.16-4,646.66 ns | 8,095.18-18,941.93 ns | 3.53-4.08x |
| tools/call | 2,324.78-6,869.06 ns | 7,640.71-25,366.39 ns | 3.29-3.69x |

A typed builder handler is another 1.26-1.34x faster than the already-fast
`Value` handler for the measured tool call.

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

- compact `ping`, `initialize`, and `tools/list` messages use an exact
  zero-allocation recognizer;
- the common compact `tools/call` layout has an exact recognizer that borrows
  its validated arguments;
- reordered, spaced, or escaped input falls back to the strict
  order-independent `JsonCursor`;
- routing fields and raw tool arguments borrow from the input line;
- typed/raw handlers skip an intermediate mutable JSON DOM;
- `ToolReply::structured` serializes an arbitrary Serde result once into a
  `RawValue`, then reuses the same bytes for text and `structuredContent`;
- responses serialize directly from borrowed structs;
- the builder lends its identity and catalog instead of cloning them;
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

`ToolServer::call_raw`, `identity_ref`, and `catalog_ref` have compatible
defaults. Existing implementations do not need to define them; optimized
implementations can override them.

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
```

CI covers Linux, Windows, macOS, and Rust 1.78.

## Name and release

As of July 28, 2026, the exact `mcport` name has no published crate in the
crates.io API. The first release is intentionally held until
`blazingly-json` is published and this repository passes its final package
verification against the registry version.

## License

MIT
