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

## July 28, 2026 published-baseline results

These archived numbers are for `mcport 0.1.0` on an Intel Core Ultra 7 255U,
Windows, Rust 1.97.1. Ranges below are from three complete runner invocations:

| Workload | Server | Median latency | Throughput | Versus mcport |
| --- | --- | ---: | ---: | ---: |
| tools/list | mcport | 4.41-5.67 us | 176,410-226,858 req/s | 1.00x |
| tools/list | rmcp | 71.12-100.79 us | 9,922-14,060 req/s | 15.97-17.78x slower |
| tools/list | rust-mcp-sdk | 107.23-139.74 us | 7,156-9,326 req/s | 24.33-25.43x slower |
| tools/call | mcport | 7.01-7.75 us | 129,047-142,698 req/s | 1.00x |
| tools/call | rmcp | 61.84-96.21 us | 10,394-16,172 req/s | 8.82-12.71x slower |
| tools/call | rust-mcp-sdk | 101.87-147.12 us | 6,797-9,816 req/s | 13.15-19.44x slower |

With the shared release profile, binary sizes were 545,792 bytes for mcport,
1,633,280 bytes for rmcp, and 1,654,272 bytes for rust-mcp-sdk.

## Scope and limits

This benchmark answers one narrow question: how much machinery is required to
serve common local MCP tool traffic over stdio? It does not compare remote
HTTP transports, clients, prompts, resources, sampling, OAuth, subscriptions,
or the broader SDK feature sets. Run it on the intended deployment hardware
and compare ratios as well as absolute numbers.
