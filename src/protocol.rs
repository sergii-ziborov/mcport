//! JSON-RPC and MCP tool-result message shapes.

use crate::ToolPayload;
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

/// Adds a JSON-RPC error data object to an error envelope.
#[must_use]
pub fn error_with_data(id: &Value, code: i64, message: impl Into<String>, data: Value) -> Value {
    let mut response = error(id, code, message);
    if let Some(error) = response.get_mut("error").and_then(Value::as_object_mut) {
        error.insert("data".to_owned(), data);
    }
    response
}

/// Marks a successful result as complete for MCP 2026-07-28.
pub(crate) fn mark_complete(response: &mut Value) {
    if let Some(result) = response.get_mut("result").and_then(Value::as_object_mut) {
        result.insert(
            "resultType".to_owned(),
            Value::String("complete".to_owned()),
        );
    }
}

/// A successful `tools/call` result carrying the representations `payload`
/// selects: text content, a `structuredContent` mirror of it, or structured
/// content alone.
#[must_use]
pub fn tool_success(id: &Value, value: &Value, payload: ToolPayload) -> Value {
    let structured = payload.is_structured() && value.is_object();
    let text = if payload.has_text() || !structured {
        let rendered = if structured {
            blazingly_json::to_string_pretty(value)
        } else {
            blazingly_json::to_string(value)
        };
        Some(rendered.unwrap_or_else(|_| "{}".to_owned()))
    } else {
        None
    };
    let mut result = json!({"content": content_block(text.as_deref()), "isError": false});
    if structured {
        if let Some(object) = result.as_object_mut() {
            object.insert("structuredContent".to_owned(), value.clone());
        }
    }
    success(id, &result)
}

/// A successful `tools/call` result built from one pre-serialized value.
#[must_use]
pub fn tool_success_raw(id: &Value, value: &RawValue, payload: ToolPayload) -> Value {
    let structured_value = if payload.is_structured() && value.get().starts_with('{') {
        blazingly_json::from_str::<Value>(value.get()).ok()
    } else {
        None
    };
    let text = (payload.has_text() || structured_value.is_none()).then(|| value.get());
    let mut result = json!({"content": content_block(text), "isError": false});
    if let (Some(object), Some(value)) = (result.as_object_mut(), structured_value) {
        object.insert("structuredContent".to_owned(), value);
    }
    success(id, &result)
}

/// One text block, or none at all when the payload carries structure only.
fn content_block(text: Option<&str>) -> Value {
    text.map_or_else(|| json!([]), |text| json!([{"type": "text", "text": text}]))
}

/// A successful MCP 2026-07-28 result with arbitrary JSON structured content.
#[must_use]
pub fn tool_success_raw_any(id: &Value, value: &RawValue) -> Value {
    let mut result = json!({
        "content": [{"type": "text", "text": value.get()}],
        "isError": false
    });
    if let (Some(object), Ok(value)) = (
        result.as_object_mut(),
        blazingly_json::from_str::<Value>(value.get()),
    ) {
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
    use crate::ToolPayload;
    use blazingly_json::json;

    #[test]
    fn shapes_match_the_mcp_contract() {
        let ok = super::tool_success(&json!(1), &json!({"nodes": 5}), ToolPayload::Mirrored);
        assert_eq!(ok["result"]["isError"], false);
        assert_eq!(ok["result"]["structuredContent"]["nodes"], 5);
        assert!(ok["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("nodes"));

        let flat = super::tool_success(&json!(2), &json!({"nodes": 5}), ToolPayload::Text);
        assert!(flat["result"].get("structuredContent").is_none());

        let scalar = super::tool_success(&json!(3), &json!(5), ToolPayload::Mirrored);
        assert!(scalar["result"].get("structuredContent").is_none());

        let raw_scalar = blazingly_json::to_raw_value(&5).unwrap();
        let modern_scalar = super::tool_success_raw_any(&json!(3), &raw_scalar);
        assert_eq!(modern_scalar["result"]["structuredContent"], 5);

        let failed = super::tool_error(&json!(4), "boom");
        assert_eq!(failed["result"]["isError"], true);

        let protocol = super::error(&json!(5), -32_601, "method not found: x");
        assert_eq!(protocol["error"]["code"], -32_601);
    }

    #[test]
    fn structured_only_drops_the_mirror_and_roughly_halves_the_response() {
        let value = json!({
            "endpoints": (0..40)
                .map(|index| json!({"method": "GET", "path": format!("/resource/{index}")}))
                .collect::<Vec<_>>()
        });

        let mirrored = super::tool_success(&json!(1), &value, ToolPayload::Mirrored);
        let structured = super::tool_success(&json!(1), &value, ToolPayload::Structured);

        assert_eq!(
            mirrored["result"]["structuredContent"], structured["result"]["structuredContent"],
            "the machine-readable half is identical"
        );
        assert!(
            structured["result"]["content"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "the mirror is gone, and the field it lived in is still present"
        );

        let mirrored_bytes = blazingly_json::to_string(&mirrored).unwrap().len();
        let structured_bytes = blazingly_json::to_string(&structured).unwrap().len();
        assert!(
            structured_bytes * 2 < mirrored_bytes,
            "the pretty-printed mirror is the larger half: {mirrored_bytes} -> {structured_bytes}"
        );
    }

    #[test]
    fn a_value_that_cannot_be_structured_keeps_its_text_block() {
        // Legacy revisions carry arrays and scalars through text only. Dropping
        // the text block there would answer with nothing at all.
        for value in [json!([1, 2, 3]), json!(7), json!("done")] {
            let reply = super::tool_success(&json!(1), &value, ToolPayload::Structured);
            assert!(
                reply["result"]["content"][0]["text"].is_string(),
                "{value} must still reach a client that reads content"
            );
            assert!(reply["result"].get("structuredContent").is_none());
        }
    }
}
