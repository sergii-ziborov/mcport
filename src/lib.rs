//! Blocking, dependency-light MCP stdio server runtime.
//!
//! MCP over stdio is a single ordered byte stream: the client writes one
//! newline-delimited JSON-RPC message, the server answers on stdout. There is
//! no multiplexing to schedule, so this crate deliberately ships no async
//! executor. The entire runtime is a blocking read loop over `std::io`, and
//! its only dependency is `serde_json`.
//!
//! Implement [`ToolServer`] for your tool surface and hand it to [`serve`]:
//!
//! ```no_run
//! use serde_json::{Value, json};
//! use weavatrix_mcp::{ServerIdentity, ToolReply, ToolServer};
//!
//! struct Echo;
//!
//! impl ToolServer for Echo {
//!     fn identity(&self) -> ServerIdentity {
//!         ServerIdentity::new("echo", "1.0.0", "Echoes tool arguments back.")
//!     }
//!
//!     fn catalog(&mut self) -> Value {
//!         json!([{
//!             "name": "echo",
//!             "description": "Echo the arguments.",
//!             "inputSchema": {"type": "object", "additionalProperties": true}
//!         }])
//!     }
//!
//!     fn call(&mut self, name: &str, arguments: Value) -> ToolReply {
//!         match name {
//!             "echo" => ToolReply::structured(arguments),
//!             _ => ToolReply::error(format!("unknown tool: {name}")),
//!         }
//!     }
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     weavatrix_mcp::serve(&mut Echo)
//! }
//! ```
//!
//! Runtime guarantees:
//!
//! - `initialize`, `ping`, and `tools/list` are answered from the catalog
//!   alone, so servers can defer expensive startup to the first `tools/call`;
//! - a UTF-8 byte-order mark on any line is stripped before parsing, so
//!   Windows shell pipelines cannot break the first request;
//! - notifications (messages without an `id`) are consumed without replies;
//! - malformed JSON, missing methods, and unknown methods produce JSON-RPC
//!   errors without terminating the loop.

pub mod protocol;

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

/// Protocol revision negotiated when the client does not request one.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// Identity block reported from `initialize`.
#[derive(Debug, Clone)]
pub struct ServerIdentity {
    /// Server name reported to the client.
    pub name: String,
    /// Server version reported to the client.
    pub version: String,
    /// One-line operating instructions shown to the model.
    pub instructions: String,
}

impl ServerIdentity {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        instructions: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            instructions: instructions.into(),
        }
    }
}

/// Outcome of one `tools/call` dispatch.
#[derive(Debug, Clone)]
pub enum ToolReply {
    /// Successful tool output.
    Success {
        /// Tool output value serialized into the text content block.
        value: Value,
        /// Whether to also attach `structuredContent`.
        structured: bool,
    },
    /// Tool failure reported as `isError: true` content, not a protocol error.
    Error(String),
}

impl ToolReply {
    /// Success carrying both text and `structuredContent`.
    #[must_use]
    pub fn structured(value: Value) -> Self {
        Self::Success {
            value,
            structured: true,
        }
    }

    /// Success carrying compact text content only.
    #[must_use]
    pub fn text(value: Value) -> Self {
        Self::Success {
            value,
            structured: false,
        }
    }

    /// Tool failure with a human-readable message.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }
}

/// A read-only tool surface served over MCP stdio.
pub trait ToolServer {
    /// Identity reported from `initialize`.
    fn identity(&self) -> ServerIdentity;

    /// Tool catalog returned from `tools/list`, as a JSON array.
    fn catalog(&mut self) -> Value;

    /// Dispatches one tool call.
    fn call(&mut self, name: &str, arguments: Value) -> ToolReply;
}

/// Serves a [`ToolServer`] over process stdin/stdout until EOF.
///
/// # Errors
///
/// Returns only stdio failures. Invalid requests are answered with JSON-RPC
/// errors and do not terminate the server.
pub fn serve(server: &mut impl ToolServer) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_streams(server, stdin.lock(), stdout.lock())
}

/// Serves a [`ToolServer`] over arbitrary streams until EOF.
///
/// This is [`serve`] with injectable transport, which makes the full loop
/// testable without spawning processes.
///
/// # Errors
///
/// Returns only stream I/O failures.
pub fn serve_streams(
    server: &mut impl ToolServer,
    reader: impl BufRead,
    mut writer: impl Write,
) -> io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(line) {
            Ok(request) => match dispatch(server, &request) {
                Some(response) => response,
                None => continue,
            },
            Err(error) => protocol::error(&Value::Null, -32_700, error.to_string()),
        };
        write_message(&mut writer, &response)?;
    }
    Ok(())
}

/// Dispatches one parsed JSON-RPC request.
///
/// Returns `None` for notifications (requests without an `id`), which must
/// not be answered.
pub fn dispatch(server: &mut impl ToolServer, request: &Value) -> Option<Value> {
    request.get("id")?;
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Some(protocol::error(&id, -32_600, "missing JSON-RPC method"));
    };
    Some(match method {
        "initialize" => {
            let version = request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            let identity = server.identity();
            protocol::success(
                &id,
                &json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": identity.name,
                        "version": identity.version
                    },
                    "instructions": identity.instructions
                }),
            )
        }
        "ping" => protocol::success(&id, &json!({})),
        "tools/list" => protocol::success(&id, &json!({"tools": server.catalog()})),
        "tools/call" => {
            let Some(name) = request.pointer("/params/name").and_then(Value::as_str) else {
                return Some(protocol::error(
                    &id,
                    -32_602,
                    "tools/call requires params.name",
                ));
            };
            let arguments = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match server.call(name, arguments) {
                ToolReply::Success { value, structured } => {
                    protocol::tool_success(&id, &value, structured)
                }
                ToolReply::Error(message) => protocol::tool_error(&id, message),
            }
        }
        _ => protocol::error(&id, -32_601, format!("method not found: {method}")),
    })
}

fn write_message(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::{dispatch, serve_streams, ServerIdentity, ToolReply, ToolServer};
    use serde_json::{json, Value};

    struct Echo {
        calls: usize,
    }

    impl ToolServer for Echo {
        fn identity(&self) -> ServerIdentity {
            ServerIdentity::new("echo", "1.2.3", "Echoes tool arguments back.")
        }

        fn catalog(&mut self) -> Value {
            json!([{
                "name": "echo",
                "description": "Echo the arguments.",
                "inputSchema": {"type": "object", "additionalProperties": true}
            }])
        }

        fn call(&mut self, name: &str, arguments: Value) -> ToolReply {
            self.calls += 1;
            match name {
                "echo" => ToolReply::structured(arguments),
                "flat" => ToolReply::text(arguments),
                _ => ToolReply::error(format!("unknown tool: {name}")),
            }
        }
    }

    #[test]
    fn negotiates_lists_and_calls() {
        let mut server = Echo { calls: 0 };
        let initialized = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2025-06-18"}
            }),
        )
        .unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(initialized["result"]["serverInfo"]["name"], "echo");
        assert_eq!(server.calls, 0, "initialize must not call tools");

        let fallback = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "initialize"}),
        )
        .unwrap();
        assert_eq!(
            fallback["result"]["protocolVersion"],
            super::DEFAULT_PROTOCOL_VERSION
        );

        let listed = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        )
        .unwrap();
        assert_eq!(listed["result"]["tools"].as_array().map(Vec::len), Some(1));

        let called = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "echo", "arguments": {"value": 7}}
            }),
        )
        .unwrap();
        assert_eq!(called["result"]["isError"], false);
        assert_eq!(called["result"]["structuredContent"]["value"], 7);

        let flat = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {"name": "flat", "arguments": {"value": 7}}
            }),
        )
        .unwrap();
        assert!(flat["result"].get("structuredContent").is_none());

        let failed = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": {"name": "missing"}
            }),
        )
        .unwrap();
        assert_eq!(failed["result"]["isError"], true);
    }

    #[test]
    fn rejects_invalid_requests_without_stopping() {
        let mut server = Echo { calls: 0 };
        let no_method = dispatch(&mut server, &json!({"jsonrpc": "2.0", "id": 1})).unwrap();
        assert_eq!(no_method["error"]["code"], -32_600);

        let unknown = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list"}),
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32_601);

        let unnamed = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call"}),
        )
        .unwrap();
        assert_eq!(unnamed["error"]["code"], -32_602);

        let notification = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        assert!(notification.is_none(), "notifications are not answered");
    }

    #[test]
    fn serves_a_full_session_over_streams() {
        let mut server = Echo { calls: 0 };
        let input = concat!(
            "\u{feff}{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n",
            "\n",
            "not json\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",",
            "\"params\":{\"name\":\"echo\",\"arguments\":{\"ok\":true}}}\n",
        );
        let mut output = Vec::new();
        serve_streams(&mut server, input.as_bytes(), &mut output).unwrap();
        let lines = String::from_utf8(output).unwrap();
        let lines = lines.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "ping, parse error, tool call");

        let ping = serde_json::from_str::<Value>(lines[0]).unwrap();
        assert_eq!(ping["id"], 1, "BOM-prefixed first request still parses");

        let parse_error = serde_json::from_str::<Value>(lines[1]).unwrap();
        assert_eq!(parse_error["error"]["code"], -32_700);

        let called = serde_json::from_str::<Value>(lines[2]).unwrap();
        assert_eq!(called["result"]["structuredContent"]["ok"], true);
        assert_eq!(server.calls, 1);
    }
}
