# mcport competitor benchmark

This is a reproducible black-box stdio comparison between:

- the current `mcport` working tree;
- `rmcp 0.16.0`, the official Rust MCP SDK package used by this harness;
- `rust-mcp-sdk 1.0.1`.

It is a separate unpublished Cargo package. The root crate excludes the entire
`benchmarks/competitors` directory, so Tokio, `serde_json`, and both competitor
SDKs are benchmark-only dependencies.

## Run

```text
cargo build --manifest-path benchmarks/competitors/Cargo.toml --release --bins
benchmarks/competitors/target/release/bench-runner
```

On Windows, append `.exe` to the runner path.

The release profile applies the same thin LTO, one codegen unit, symbol
stripping, and abort-on-panic settings to all server binaries.

## Current-versus-baseline regression gate

Set `MCPORT_BASELINE_SERVER` to a previously built mcport server executable to
insert it directly after the current server. The runner uses nine measured
rounds, alternates process order, and reports the median of the paired
current/baseline latency ratios. Set `MCPORT_BASELINE_ONLY` to omit the two
competitor SDKs, and `MCPORT_MAX_REGRESSION_PERCENT=0` to fail on any measured
regression:

```text
MCPORT_BASELINE_SERVER=/absolute/path/to/mcport-server \
MCPORT_BASELINE_ONLY=1 \
MCPORT_MAX_REGRESSION_PERCENT=0 \
benchmarks/competitors/target/release/bench-runner
```

On PowerShell, set the three names through `$env:` before invoking
`bench-runner.exe`. Use the same compiler, release profile, hardware, and
background-load conditions for both binaries.

Set `MCPORT_MIN_COMPETITOR_RATIO=1.0` to fail if either competitor completes a
workload faster than mcport. CI runs this guard on the complete black-box
workload, with identical release settings and semantic/response-count checks.

## Method

For each workload, the runner:

1. starts a fresh server process;
2. writes a valid MCP `2025-11-25` initialize request and initialized
   notification;
3. writes 10,000 newline-delimited requests with unique numeric IDs;
4. reads stdout concurrently, keeping stdin open until all responses arrive;
5. rejects protocol errors and requires exactly 10,001 responses;
6. validates the final warmup response semantically;
7. warms every implementation once;
8. rotates implementation order across five measured rounds;
9. reports the median full-process time divided by 10,000.

Timing starts before process spawn and ends when the final expected response
arrives. It therefore includes process startup, OS pipe transfer, runtime and
codec work, dispatch, handler execution, and response serialization. Input
construction and semantic warmup validation are outside measured rounds.

The same `query_graph` schema, deterministic arguments, and structured result
are used for all implementations. Response byte counts are printed so a
smaller or incomplete output cannot silently masquerade as a speedup.

## July 29, 2026 published results

These numbers are for published `mcport 0.3.0` on an Intel Core Ultra 7 255U,
Windows MSVC, Rust 1.97.1. Ranges below are from three complete runner
invocations:

| Workload | Server | Median latency | Throughput | Versus mcport |
| --- | --- | ---: | ---: | ---: |
| tools/list | mcport | 5.41-9.13 us | 109,473-184,988 req/s | 1.00x |
| tools/list | rmcp | 91.72-218.26 us | 4,582-10,903 req/s | 16.97-23.89x slower |
| tools/list | rust-mcp-sdk | 133.98-253.22 us | 3,949-7,464 req/s | 24.78-28.31x slower |
| tools/call | mcport | 7.39-9.88 us | 101,233-135,296 req/s | 1.00x |
| tools/call | rmcp | 82.31-112.96 us | 8,853-12,149 req/s | 8.92-11.44x slower |
| tools/call | rust-mcp-sdk | 121.31-152.15 us | 6,573-8,243 req/s | 13.15-16.42x slower |

Five additional stabilized, nine-round paired invocations compared `0.3.0`
against the optimized `0.1.0` baseline built with the same MSVC toolchain.
The median invocation improved `tools/list` latency by 4.57% and `tools/call`
latency by 6.05%. All five `tools/list` invocations improved; four of five
`tools/call` invocations improved, with the invocation-level range spanning
-14.68% to +3.85%.

With the shared MSVC thin-LTO release profile, binary sizes were 411,136 bytes
for mcport, 1,440,256 bytes for rmcp, and 1,469,440 bytes for rust-mcp-sdk.

## Scope and limits

This benchmark answers one narrow question: how much machinery is required to
serve common local MCP tool traffic over stdio? It does not compare remote
HTTP transports, clients, prompts, resources, sampling, OAuth, subscriptions,
or the broader SDK feature sets. Run it on the intended deployment hardware
and compare ratios as well as absolute numbers.
