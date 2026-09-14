# Changelog

## 0.6.0

- Keep speed as the first README claim and fix install/version drift. The
  published SDK comparison stays attributed to mcport 0.3.0; it is not
  relabeled as 0.5.0 or 0.6.0.
- Add `#[tool]`, `tools!`, `IntoToolReply`, `ToolError`, `Json<T>`,
  `ToolDefinition`, and `ToolReply::literal_text` without changing
  `ToolReply::text`.
- Serialize the tool catalog once after registration instead of on every
  `tool()` call.
- Keep borrowed routing for valid modern `tools/list` and `tools/call`
  requests that carry protocol metadata.

## 0.5.0

- Add `ToolReply::structured_only` / `ToolPayload::Structured`.
