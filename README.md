# mcport

**Faster than every SDK in our published stdio comparison. No Tokio.**

A small Rust MCP server runtime for local tools and existing synchronous
applications. Ordinary Rust functions become MCP tools — without handwritten
JSON wrapping, without Tokio, and without extra work on every call.

The recorded comparison covers rmcp 0.16.0 and rust-mcp-sdk 1.0.1 on a
specific tools-only Windows stdio workload. It is not a measurement of every
SDK, transport, platform, or execution mode. See [Performance](#performance)
for versions, platform, and method.

## Start with a working project

MCPort is a library. Add it to the Rust binary that will serve your tools.

```text
cargo new my-mcp-server
cd my-mcp-server
cargo add mcport@0.6.1
cargo add serde --features derive
```

The core crate declares Rust 1.78 as its minimum supported version. The
default `macros` feature is compile-time only: it does not add Tokio, Hyper,
Axum, or `serde_json`.

Replace `src/main.rs` with:

```rust
use mcport::{tool, tools, McpServer};

/// Adds two integers.
#[tool]
fn add(left: i32, right: i32) -> i64 {
    i64::from(left) + i64::from(right)
}

fn main() -> std::io::Result<()> {
    McpServer::new("math", env!("CARGO_PKG_VERSION"))
        .tools(tools![add])
        .serve()
}
```

`add(2, 3)` remains an ordinary Rust call. The attribute adds metadata and an
adapter; it does not turn the function into an object you cannot use outside
MCP. The existing `.tool(name, description, schema, handler)` path is
unchanged.

The same server lives in [`examples/math.rs`](examples/math.rs). A handwritten
typed handler, without the macro, is [`examples/typed_manual.rs`](examples/typed_manual.rs).

Build and run your server:

```text
cargo build --release
```

Configure your MCP client's stdio integration with the absolute path to your
application's executable, not a command named `mcport`:

```text
/path/to/my-mcp-server/target/release/my-mcp-server
```

On Windows the executable has the `.exe` suffix. A server waiting for stdin
normally prints nothing. Logging and diagnostics belong on stderr; stdout is
reserved for MCP frames. Keep process arguments separate from the executable
path in the client's configuration.

A manual legacy conversation consists of one JSON object per line:

```text
{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"manual-test","version":"1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":1,"method":"tools/list"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"add","arguments":{"left":2,"right":3}}}
```

The `add` result contains `structuredContent`: `{"result":5}`. There is no
response to the `initialized` notification.

## Choose the execution mode

| Need | Start here | Important boundary |
| --- | --- | --- |
| Small synchronous tools, mutable application state, minimum overhead | `McpServer` | Handlers run inline; a slow handler blocks further input processing. |
| Bounded parallel handlers, cooperative cancellation, deadlines, progress | `ConcurrentMcpServer` + `RuntimeConfig` | Cancellation is cooperative. Arbitrary Rust threads cannot safely be forcibly killed. |
| An application that already owns framing or transport | `ToolServer` + `serve_message` | The embedding transport must enforce framing, byte limits, flushing and shutdown. |

Do not choose controlled execution just to get typed arguments: the inline
builder already supports `#[tool]`, `typed_tool`, and `raw_tool`.

## Existing applications do not need a new JSON engine everywhere

Use your existing Serde models at the MCP boundary. A handler can deserialize
into your input structure and pass a serializable output to
`ToolReply::structured`. Your other dependencies may continue using their own
`serde_json::Value` types.

Use `raw_tool` when your application needs its own decoding or direct access
to validated JSON. This avoids forcing a full application-wide migration to
MCPort's public `Value` type.

The optional migration technique of aliasing the `serde_json` dependency name
to `blazingly-json` is a separate, advanced change. It must not be confused
with mandatory installation of MCPort, nor with guaranteed type identity
across other libraries' APIs.

## State and startup work

`McpServer::with_state` and the `*_with_state` registration methods share
mutable state between inline calls. The controlled runtime requires an
explicit thread-safe state model.

A cheap identity/catalog lets discovery avoid expensive tool work. It does
not defer work your program has already executed while constructing state:
`with_state(..., load_index())` still calls `load_index()` first. Use an
explicit lazy state holder when loading belongs on the first tool call. See
[`examples/lazy_state.rs`](examples/lazy_state.rs).

To add `--mcp` to an existing CLI and keep the same functions callable from
the command line, see [`examples/embed_cli.rs`](examples/embed_cli.rs).

## Results and errors

| API | Result |
| --- | --- |
| `String` / `&'static str` from `#[tool]` | Literal text block, no extra JSON-quote layer |
| number / `bool` / `Vec<T>` from `#[tool]` | Stable object wrapper `{"result": ...}` |
| struct with `#[derive(ToolOutput)]` | Object result + `structuredContent` |
| `Result<T, ToolError>` | Success `T` or a tool-level failure; not `{ "Ok": ... }` |
| `Json<T>` | Explicit JSON mode for a `Serialize` value |
| `ToolReply::structured(value)` | Object results include text plus `structuredContent` |
| `ToolReply::structured_only(value)` | Object results omit the text mirror. Use only with a client known to read structured content. |
| `ToolReply::text(value)` | Serializes the value as JSON and puts that JSON representation in a text block. It is **not** a literal-text constructor. |
| `ToolReply::literal_text(text)` | Literal text block. Additive; does not change `text()`. |
| `ToolReply::error(message)` | A tool-level result with `isError: true`, not a JSON-RPC transport error. |

`ToolReply::text("hello")` produces text containing the JSON representation
`"hello"`, including its JSON string quotes. Do not assume literal string
semantics.

Text mirroring is the default compatibility behavior. Removing it can
substantially reduce wire bytes, but does not establish any particular
reduction in the model's input tokens. Some clients ignore
`structuredContent`; they would see an empty result for structured-only
object output.

The non-object `StructuredAny` path does not preserve an independently
chosen payload policy. Test arrays/scalars and the exact client/protocol
combination before relying on structured-only output there. Native modern
non-object output is an explicit path, not a silent shape change.

## Schema checks

Generated `#[tool]` schemas use `additionalProperties: false` and apply
`default` / `min` / `max` both in the advertised schema and in the adapter.
A missing `limit` with `default = 20` becomes 20. A sent `null` is not
treated as a missing field.

`strict_schemas()` turns recorded description defects into startup errors. It
does not constitute full JSON Schema validation or proof that a handwritten
schema matches a Rust type. The current lint has known coverage limits around
composition/reference keywords. Treat improvements to the lint as correctness
work, not as a reason to advertise broader guarantees.

`read_only` and `closed_world` are advertised hints. They do not sandbox the
function. MCP also requires clients to treat tool annotations as untrusted
when the server is untrusted.

## Protocol and scope

The crate implements the tools-oriented server surface for legacy
initialization (`2025-06-18`, `2025-11-25`) and a modern per-request protocol
path (`2026-07-28`). Modern structured results can carry non-object JSON;
legacy non-object results use the text compatibility path. Valid modern
`tools/list` and `tools/call` requests keep borrowed routing; they no longer
fall back to a full owned request DOM only because `_meta` is present.

Runtime-neutral extension hooks exist for additional methods and capability
composition. Resources/prompts are not automatically exposed through a full
high-level builder API.

There is no built-in remote HTTP transport, OAuth service, production client,
browser dashboard or arbitrary-code sandbox. These are not pulled into the
default runtime dependency graph.

## Bounds and cancellation

The standard stream adapters bound input/output messages and construct
responses atomically rather than emitting a truncated JSON document after a
response-size overflow. Defaults are 8 MiB per request and response.

| Bound | What it limits | What it does not limit |
| --- | --- | --- |
| Request/response bytes | One MCP frame | Process RSS, handler allocations, intermediate `Value`s |
| Controlled queue depths | Waiting jobs and output frames | A blocked writer, or shutdown time |
| `max_in_flight` | Concurrent handler slots | A timed-out handler that ignores cancellation still occupies its slot until it exits |

`FlushPolicy::PerMessage` is the interactive default. Count-only batching can
stall a client that sends one request and waits for its reply.

## Performance

MCPort spends less work on local MCP requests: no mandatory async executor,
borrowed request routing, typed/raw/`#[tool]` handlers, a catalog serialized
once after registration, and direct response serialization on its optimized
path.

Two kinds of comparison must remain separate:

- **Internal fast path vs owned DOM:** both implementations use MCPort and
  the same JSON engine. These numbers measure architectural changes within
  the runtime, not the removal of Tokio.
- **SDK comparison:** complete server binaries exchanging messages over real
  stdin/stdout pipes. These include runtime, codec, dispatch, handler, OS
  pipe and process startup costs.

### Recorded SDK comparison

These numbers are for **published mcport 0.6.0** (tag `v0.6.0`, commit
`c057d6c`, 14 September 2026) on Intel Core Ultra 7 255U / Windows MSVC /
rustc 1.97.1, against rmcp 0.16.0 and rust-mcp-sdk 1.0.1. Ranges are from
three complete runner invocations. This crate revision only rebinds that
measurement; the runtime is the same 0.6.0 code. A 29 July 2026 table for
0.3.0 was previously the published claim, and was once mislabeled `0.4.0`;
`0.4.0` was never published to crates.io.

The runner reports the median total time of a pipelined series divided by
10,000. This is amortized time per request, not a p50 individual round-trip
latency.

| Workload | Compared SDK | Recorded slowdown relative to MCPort |
| --- | --- | ---: |
| tools/list | rmcp 0.16.0 | 12.42–14.72× |
| tools/list | rust-mcp-sdk 1.0.1 | 17.76–23.37× |
| tools/call | rmcp 0.16.0 | 7.91–11.11× |
| tools/call | rust-mcp-sdk 1.0.1 | 11.91–13.64× |

Reproduce the checked-in harness from a repository checkout. On Windows, the
published method uses the MSVC host toolchain:

```text
cargo +stable-x86_64-pc-windows-msvc build --locked --manifest-path benchmarks/competitors/Cargo.toml --release --bins
benchmarks/competitors/target/release/bench-runner
```

On Windows append `.exe` to the runner. The competitor workspace is excluded
from the published crate and does not become a dependency of your
application. Full method, binary sizes, and scope:
[`benchmarks/competitors/README.md`](benchmarks/competitors/README.md).

### Internal fast path

`benches/runtime.rs` compares `serve_message` with an owned-DOM `dispatch`
loop. Both sides use `blazingly-json`. The published `0.1.0` baseline on the
same Windows machine was 8.9–12.5× for `initialize` and 3.6–4.0× for
`tools/call`. Run `cargo bench --bench runtime` on the deployment machine
before using absolute nanoseconds for capacity planning.

## Dependencies and safety

Normal direct runtime dependencies are `blazingly-json` and `serde`. The
optional `macros` feature adds `mcport-macros` at compile time only. An
optional Schemars bridge is not included: Schemars 1.2.2 depends on
`serde_json` even with default features off.

The MCPort crate forbids `unsafe` code; this is a property of MCPort itself,
not a claim that every transitive dependency has no unsafe code.

## Verification

```text
cargo fmt --all -- --check
cargo test --locked --all-features
cargo test --locked --features subprocess-tests --test stdio_subprocess
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.78 build --locked --all-features
```

The repository has Linux, Windows and macOS CI and subprocess tests. That
does not replace testing your own handlers, client integration or
platform-specific shutdown behavior.

An Inspector smoke test should target **your** binary, not the repository
fixture:

```text
cargo build --release --example math
npx -y @modelcontextprotocol/inspector@latest --cli \
  target/release/examples/math --method tools/list
```

## License

MIT
