# Changelog

## 0.6.1

- Rebind the published stdio SDK comparison to three Windows MSVC runner
  invocations of mcport 0.6.0. The runtime is unchanged.

## 0.6.0

- Keep speed as the first README claim and fix install/version drift. The
  crate shipped with the July 2026 0.3.0 SDK table still as the published
  claim; 0.6.1 replaces that table with a measured 0.6.0 rerun.
- Add `#[tool]`, `tools!`, `IntoToolReply`, `ToolError`, `Json<T>`,
  `ToolDefinition`, and `ToolReply::literal_text` without changing
  `ToolReply::text`.
- Serialize the tool catalog once after registration instead of on every
  `tool()` call.
- Keep borrowed routing for valid modern `tools/list` and `tools/call`
  requests that carry protocol metadata.

## 0.5.0

- Add `ToolReply::structured_only` / `ToolPayload::Structured`.
