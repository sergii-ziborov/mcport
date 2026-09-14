use mcport::{json, McpServer, ToolReply};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Add {
    left: i64,
    right: i64,
}

#[derive(Serialize)]
struct Sum {
    value: i64,
}

fn main() -> std::io::Result<()> {
    McpServer::new("math", env!("CARGO_PKG_VERSION"))
        .strict_schemas()
        .typed_tool(
            "add",
            "Adds two signed 64-bit integers. Returns an error on overflow.",
            json!({
                "type": "object",
                "properties": {
                    "left": {"type": "integer"},
                    "right": {"type": "integer"}
                },
                "required": ["left", "right"],
                "additionalProperties": false
            }),
            |arguments: Add| match arguments.left.checked_add(arguments.right) {
                Some(value) => ToolReply::structured(Sum { value }),
                None => ToolReply::error("sum exceeds the signed 64-bit range"),
            },
        )
        .serve()
}
