# MCPort roadmap

Date: 14 September 2026. This document actualizes the
speed-plus-FastMCP-ergonomics plan against the current tree.

## Product decision

Do not replace speed with convenience. Convenience must generate the same
cheap synchronous path: a Rust function, compile-time adapter, prepared
catalog, existing MCPort dispatch.

Public claim today:

> Faster than every SDK in our published stdio comparison. No Tokio.

After the 0.6.0 convenience surface:

> Ordinary Rust functions. FastMCP-like ergonomics. Native Rust speed.

## Status versus the staged plan

| Stage | Intent | Status in this tree |
| --- | --- | --- |
| A | README, versions, benchmark provenance, standalone quickstart | Done. Install is `0.6.1`. The published SDK table is the 14 September 2026 three-run MSVC measurement of 0.6.0 (`v0.6.0` / `c057d6c`). Examples are complete files. |
| B | Protocol boundaries, descriptor, clear results/errors | Done for the first cut: `ToolDefinition`, annotations, `literal_text`, `ToolError` / `IntoToolReply` / `Json<T>`. |
| C | Optional `#[tool]`, `tools!`, limited schema generation | Done as the default `macros` feature. `#[context]` is deferred. Schemars is not a runtime dependency. |
| D | Modern fast path, catalog finalize, macro/manual guard | Modern `tools/list` and `tools/call` keep borrowed routing. Catalog bytes are serialized once after registration. Macro/manual parity tests exist; numeric regression thresholds are not frozen yet. |
| E | Toolsets, test helper crate, developer CLI | Not started. `serve_message` remains the in-process test hook. |
| F | Content/resources/prompts adapters | Not started. The low-level `MethodReply` hook remains. |

## Still proposed, not shipped

- `#[context]` injection and shared/concurrent state models beyond `&` / `&mut`
- toolsets with namespaces and a single registry for list + call
- `cargo mcport` (`inspect`, `call`, `doctor`, `config`)
- sequential RTT / p50-p99 harness (the published runner stays amortized)
- controlled worker pool (one thread per admitted job remains)
- optional Schemars bridge behind an explicit feature that admits `serde_json`

## Acceptance that 0.6.0 covers

- RELEASE-01 / DOC-01 / DOC-02: README version matches the crate; examples are
  complete files; typed manual example has a direct Serde import.
- MACRO-01, MACRO-02, MACRO-03, SCHEMA-01, SCHEMA-03, SCHEMA-04, OUTPUT-01,
  OUTPUT-02, PERF-02, PERF-03: covered by `tests/macros.rs` and the modern
  fixtures in `fast_message_path_preserves_public_dispatch_semantics`.

## Sources checked with this actualization

Repository `main` at the start of the work was
`9a8a8dace106534126c2be10fa862fe0b0cc1f89`. crates.io had 0.1.0, 0.2.0, 0.3.0,
0.5.0 and not 0.4.0. `0.4.0` exists only as commit `fc09087` without a
published tag.
