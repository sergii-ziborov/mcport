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
cargo +stable-x86_64-pc-windows-msvc build --locked --manifest-path benchmarks/competitors/Cargo.toml --release --bins
benchmarks/competitors/target/release/bench-runner
```

On Windows, append `.exe` to the runner path. The published 0.6.0 table used
this MSVC host toolchain (`rustc 1.97.1`). A GNU default target is a
different measurement.

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

## 14 September 2026 published results

These numbers are for published `mcport 0.6.0` (git tag `v0.6.0`, commit
`c057d6c`) on an Intel Core Ultra 7 255U, Windows MSVC, rustc 1.97.1. The
harness used the current 0.6.0 tree (`d90b712`; publish-workflow commits
only) against rmcp 0.16.0 and rust-mcp-sdk 1.0.1. Ranges below are from
three complete runner invocations:

| Workload | Server | Median latency | Throughput | Versus mcport |
| --- | --- | ---: | ---: | ---: |
| tools/list | mcport | 2.73-3.08 us | 325,135-365,763 req/s | 1.00x |
| tools/list | rmcp | 33.95-43.47 us | 23,007-29,454 req/s | 12.42-14.72x slower |
| tools/list | rust-mcp-sdk | 48.54-69.02 us | 14,489-20,600 req/s | 17.76-23.37x slower |
| tools/call | mcport | 3.68-4.89 us | 204,589-271,479 req/s | 1.00x |
| tools/call | rmcp | 34.24-40.93 us | 24,431-29,208 req/s | 7.91-11.11x slower |
| tools/call | rust-mcp-sdk | 50.24-58.19 us | 17,184-19,903 req/s | 11.91-13.64x slower |

With the shared MSVC thin-LTO release profile, binary sizes were 427,520
bytes for mcport, 1,440,768 bytes for rmcp, and 1,470,976 bytes for
rust-mcp-sdk.

### Invocations

Each line is that invocation's median full-process time / 10,000.

1. `tools/list` mcport 2734.01 ns, rmcp 12.42x, rust-mcp-sdk 17.76x; `tools/call` mcport 4887.85 ns, rmcp 7.91x, rust-mcp-sdk 11.91x
2. `tools/list` mcport 3075.65 ns, rmcp 12.73x, rust-mcp-sdk 19.52x; `tools/call` mcport 4136.61 ns, rmcp 8.28x, rust-mcp-sdk 12.30x
3. `tools/list` mcport 2953.71 ns, rmcp 14.72x, rust-mcp-sdk 23.37x; `tools/call` mcport 3683.52 ns, rmcp 11.11x, rust-mcp-sdk 13.64x

## Historical 29 July 2026 results (mcport 0.3.0)

The previous published claim used `v0.3.0` / `99c8817` on the same class of
machine and toolchain. It is no longer the primary table. Ranges then were
16.97-23.89x / 24.78-28.31x for `tools/list` and 8.92-11.44x / 13.15-16.42x
for `tools/call` versus rmcp 0.16.0 and rust-mcp-sdk 1.0.1. Five paired
invocations against the optimized 0.1.0 baseline improved median `tools/list`
latency by 4.57% and `tools/call` by 6.05%.

## Scope and limits

This benchmark answers one narrow question: how much machinery is required to
serve common local MCP tool traffic over stdio? It does not compare remote
HTTP transports, clients, prompts, resources, sampling, OAuth, subscriptions,
or the broader SDK feature sets. Run it on the intended deployment hardware
and compare ratios as well as absolute numbers.
