//! JSON-RPC and MCP tool-result message shapes.

use blazingly_json::{json, RawValue, Value};

/// A JSON-RPC success envelope.
#[must_use]
pub fn success(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A JSON-RPC error envelope.
#[must_use]
pub fn error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()}
    })
}

/// A successful `tools/call` result with text content, and, when
/// `structured` is set, a `structuredContent` mirror of the same value.
#[must_use]
pub fn tool_success(id: &Value, value: &Value, structured: bool) -> Value {
    let structured = structured && value.is_object();
    let text = if structured {
        blazingly_json::to_string_pretty(value)
    } else {
        blazingly_json::to_string(value)
    }
    .unwrap_or_else(|_| "{}".to_owned());
    let mut result = json!({
        "content": [{"type": "text", "text": text}],
        "isError": false
    });
    if structured {
        if let Some(object) = result.as_object_mut() {
            object.insert("structuredContent".to_owned(), value.clone());
        }
    }
    success(id, &result)
}

/// A successful `tools/call` result built from one pre-serialized value.
#[must_use]
pub fn tool_success_raw(id: &Value, value: &RawValue, structured: bool) -> Value {
    let mut result = json!({
        "content": [{"type": "text", "text": value.get()}],
        "isError": false
    });
    let structured_value = if structured && value.get().starts_with('{') {
        blazingly_json::from_str::<Value>(value.get()).ok()
    } else {
        None
    };
    if let (Some(object), Some(value)) = (result.as_object_mut(), structured_value) {
        object.insert("structuredContent".to_owned(), value);
    }
    success(id, &result)
}

/// A failed `tools/call` result. Tool failures are content-level errors, not
/// JSON-RPC protocol errors, so agents can read and react to them.
#[must_use]
pub fn tool_error(id: &Value, message: impl Into<String>) -> Value {
    success(
        id,
        &json!({
            "content": [{"type": "text", "text": message.into()}],
            "isError": true
        }),
    )
}

#[cfg(test)]
mod tests {
    use blazingly_json::json;

    #[test]
    fn shapes_match_the_mcp_contract() {
        let ok = super::tool_success(&json!(1), &json!({"nodes": 5}), true);
        assert_eq!(ok["result"]["isError"], false);
        assert_eq!(ok["result"]["structuredContent"]["nodes"], 5);
        assert!(ok["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("nodes"));

        let flat = super::tool_success(&json!(2), &json!({"nodes": 5}), false);
        assert!(flat["result"].get("structuredContent").is_none());

        let scalar = super::tool_success(&json!(3), &json!(5), true);
        assert!(scalar["result"].get("structuredContent").is_none());

        let failed = super::tool_error(&json!(4), "boom");
        assert_eq!(failed["result"]["isError"], true);

        let protocol = super::error(&json!(5), -32_601, "method not found: x");
        assert_eq!(protocol["error"]["code"], -32_601);
    }
}
