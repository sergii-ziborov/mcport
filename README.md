# weavatrix-mcp

Blocking, dependency-light MCP stdio server runtime. No async executor, no
tokio, no futures - the only dependency is `serde_json`, and `unsafe_code`
is forbidden.

## Why no async runtime

MCP over stdio is a single ordered byte stream: the client writes one
newline-delimited JSON-RPC message and the server answers on stdout. There is
nothing to multiplex, so an executor adds dependency surface and latency
without adding capability. Supply-chain scanners score this directly: a
blocking `std::io` loop with one dependency audits in minutes.

## What the runtime handles

- `initialize` / `ping` / `tools/list` answered from the catalog alone, so a
  server can defer expensive startup to the first `tools/call` and the
  handshake is instant on workloads of any size;
- UTF-8 BOM stripping, so Windows shell pipelines cannot break the first
  request;
- notifications consumed without replies; malformed JSON, missing methods,
  and unknown methods answered as JSON-RPC errors without terminating;
- `structuredContent` mirroring for JSON tool output, plain text otherwise;
- MCP protocol revision `2025-06-18` negotiated by default.

## Usage

Implement `ToolServer` (three methods: `identity`, `catalog`, `call`) and pass
it to `weavatrix_mcp::serve`. See the crate documentation for a complete
example. `serve_streams` exposes the same loop over injectable reader/writer
streams so the full transport is unit-testable without spawning processes.

## Who uses it

- [weavatrix-rust](https://github.com/sergii-ziborov/weavatrix-rust) - the
  Weavatrix repository-intelligence MCP.
- radiochron-mcp (planned) - replacing its tokio-based transport so the MCP
  layer carries no async runtime.

## License

MIT
