//! Blocking, dependency-light MCP stdio server runtime.
//!
//! MCP over stdio is a single ordered byte stream: the client writes one
//! newline-delimited JSON-RPC message, the server answers on stdout. There is
//! no need for an async executor: the inline adapter is a blocking bounded
//! read loop, while the optional controlled adapter uses fixed standard-library
//! worker threads and one ordered writer. The protocol path uses
//! `blazingly-json` without Tokio, Hyper, or Axum.
//!
//! Small servers can register tools directly:
//!
//! ```no_run
//! use mcport::{json, McpServer, ToolReply};
//!
//! fn main() -> std::io::Result<()> {
//!     let mut server = McpServer::new("echo", "1.0.0")
//!         .instructions("Echoes tool arguments back.")
//!         .tool(
//!             "echo",
//!             "Echo the arguments.",
//!             json!({"type": "object", "additionalProperties": true}),
//!             ToolReply::structured,
//!         );
//!     server.serve()
//! }
//! ```
//!
//! Implement [`ToolServer`] when dispatch needs a fully custom static surface:
//!
//! ```no_run
//! use mcport::{json, ServerIdentity, ToolReply, ToolServer, Value};
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
//!     mcport::serve(&mut Echo)
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
//!   errors without terminating the loop;
//! - stream adapters bound request and response bytes and never emit a partial
//!   JSON response when the output budget is exceeded.

mod builder;
mod controlled;
mod fast;
pub mod protocol;
mod transport;

pub use blazingly_json::{json, Map, RawJson, RawValue, Value};
pub use builder::McpServer;
pub use controlled::{
    serve_controlled, serve_controlled_streams, CancellationToken, ConcurrentMcpServer,
    ConcurrentToolServer, RequestContext, RuntimeConfig,
};
pub use transport::{FlushPolicy, TransportConfig, TransportLimits};

use serde::Serialize;
use std::io::{self, BufRead, Write};

/// Latest legacy handshake revision implemented by the runtime.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";

/// Stateless per-request protocol revision implemented by the runtime.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// Protocol revisions accepted by this runtime.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &[MODERN_PROTOCOL_VERSION, "2025-11-25", "2025-06-18"];

const SUPPORTED_LEGACY_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];

/// Chooses a supported protocol revision for an initialize response.
///
/// A supported client revision is echoed. Unknown or missing revisions receive
/// the latest stable revision so the client can decide whether to continue.
#[must_use]
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|requested| {
            SUPPORTED_LEGACY_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .find(|supported| *supported == requested)
        })
        .unwrap_or(DEFAULT_PROTOCOL_VERSION)
}

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
    /// Pre-serialized tool output used by the fast response path.
    Serialized {
        /// One complete validated JSON value.
        value: Box<RawValue>,
        /// Whether to also attach the object as `structuredContent`.
        structured: bool,
    },
    /// Pre-serialized non-object structured output for MCP 2026-07-28.
    ///
    /// Legacy revisions still expose this value through text content only.
    StructuredAny {
        /// One complete validated JSON scalar or array.
        value: Box<RawValue>,
    },
    /// Tool failure reported as `isError: true` content, not a protocol error.
    Error(String),
}

impl ToolReply {
    /// Success carrying both text and `structuredContent`.
    ///
    /// Any serializable result is accepted; handlers do not need to build a
    /// JSON [`Value`] first.
    #[must_use]
    pub fn structured(value: impl Serialize) -> Self {
        Self::success(value, true)
    }

    /// Success carrying compact text content only.
    ///
    /// Any serializable result is accepted.
    #[must_use]
    pub fn text(value: impl Serialize) -> Self {
        Self::success(value, false)
    }

    /// Serializes a successful tool result once.
    ///
    /// Object-shaped output is structured in every supported revision.
    /// Arrays and scalars are structured in MCP 2026-07-28 and remain
    /// text-only when serving a legacy revision.
    #[must_use]
    pub fn success(value: impl Serialize, structured: bool) -> Self {
        match blazingly_json::to_raw_value(&value) {
            Ok(value) if structured && !value.get().starts_with('{') => {
                Self::StructuredAny { value }
            }
            Ok(value) => Self::Serialized { structured, value },
            Err(error) => Self::Error(format!("tool result serialization failed: {error}")),
        }
    }

    /// Tool failure with a human-readable message.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }
}

/// One cursor-addressed page returned from `tools/list`.
#[derive(Debug, Clone)]
pub struct ToolPage {
    /// Tool descriptors in this page.
    pub tools: Value,
    /// Opaque cursor for the next page.
    pub next_cursor: Option<String>,
}

impl ToolPage {
    /// Creates a complete, non-paginated tool list.
    #[must_use]
    pub const fn complete(tools: Value) -> Self {
        Self {
            tools,
            next_cursor: None,
        }
    }
}

/// A read-only tool surface served over MCP stdio.
pub trait ToolServer {
    /// Identity reported from `initialize`.
    fn identity(&self) -> ServerIdentity;

    /// Borrows the identity when the implementation stores it.
    ///
    /// The default preserves source compatibility with existing
    /// implementations. Returning a reference avoids three string clones
    /// during initialization.
    fn identity_ref(&self) -> Option<&ServerIdentity> {
        None
    }

    /// Tool catalog returned from `tools/list`, as a JSON array.
    fn catalog(&mut self) -> Value;

    /// Borrows the catalog when the implementation stores it.
    ///
    /// Returning a reference avoids cloning the complete schema list.
    fn catalog_ref(&mut self) -> Option<&Value> {
        None
    }

    /// Borrows a compact, validated serialization of the catalog.
    ///
    /// Builder-backed servers use this to splice immutable schemas directly
    /// into `tools/list` responses instead of serializing the same catalog on
    /// every request.
    fn catalog_raw_ref(&mut self) -> Option<&RawValue> {
        None
    }

    /// Reports whether `tools/list` should use [`ToolServer::catalog_page`].
    fn catalog_is_paginated(&self) -> bool {
        false
    }

    /// Returns one cursor-addressed tool page.
    fn catalog_page(&mut self, cursor: Option<&str>) -> Result<ToolPage, String> {
        if cursor.is_some() {
            return Err("invalid tools/list cursor".to_owned());
        }
        Ok(ToolPage::complete(self.catalog()))
    }

    /// Reports whether a tool name is registered when the server can know
    /// without invoking it.
    ///
    /// Returning `Some(false)` lets mcport emit a protocol-level invalid
    /// params error for an unknown tool, as required by MCP. The compatibility
    /// default leaves custom dispatch implementations authoritative.
    fn has_tool(&self, _name: &str) -> Option<bool> {
        None
    }

    /// Dispatches one tool call.
    fn call(&mut self, name: &str, arguments: Value) -> ToolReply;

    /// Dispatches validated raw arguments without constructing a JSON DOM.
    ///
    /// Existing implementations inherit a compatible default that decodes a
    /// [`Value`] and calls [`ToolServer::call`]. Typed builders override this
    /// method and deserialize directly into the handler input.
    fn call_raw(&mut self, name: &str, arguments: RawJson<'_>) -> ToolReply {
        match arguments.deserialize::<Value>() {
            Ok(arguments) => self.call(name, arguments),
            Err(error) => ToolReply::error(format!("invalid arguments for {name}: {error}")),
        }
    }
}

/// Serves a [`ToolServer`] over process stdin/stdout until EOF.
///
/// # Errors
///
/// Returns only stdio failures. Invalid requests are answered with JSON-RPC
/// errors and do not terminate the server.
pub fn serve(server: &mut impl ToolServer) -> io::Result<()> {
    serve_with_limits(server, TransportLimits::default())
}

/// Serves a [`ToolServer`] over process stdin/stdout with explicit byte budgets.
///
/// # Errors
///
/// Returns stream failures or an invalid limits configuration.
pub fn serve_with_limits(server: &mut impl ToolServer, limits: TransportLimits) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_streams_with_limits(server, stdin.lock(), stdout.lock(), limits)
}

/// Serves a [`ToolServer`] over process stdin/stdout with complete transport
/// policy, including opt-in response batching.
///
/// # Errors
///
/// Returns stream failures or an invalid transport configuration.
pub fn serve_with_config(server: &mut impl ToolServer, config: TransportConfig) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    match config.flush_policy {
        FlushPolicy::PerMessage => {
            serve_streams_with_config(server, stdin.lock(), stdout.lock(), config)
        }
        FlushPolicy::Batch { .. } => serve_streams_with_config(
            server,
            stdin.lock(),
            io::BufWriter::new(stdout.lock()),
            config,
        ),
    }
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
    writer: impl Write,
) -> io::Result<()> {
    serve_streams_with_limits(server, reader, writer, TransportLimits::default())
}

/// Serves a [`ToolServer`] over arbitrary streams with explicit byte budgets.
///
/// Responses are assembled in a reusable bounded buffer before they become
/// visible on the output stream, so a response budget failure never emits a
/// truncated JSON document.
///
/// # Errors
///
/// Returns stream failures or an invalid limits configuration.
pub fn serve_streams_with_limits(
    server: &mut impl ToolServer,
    reader: impl BufRead,
    writer: impl Write,
    limits: TransportLimits,
) -> io::Result<()> {
    transport::serve_streams_bounded(server, reader, writer, limits)
}

/// Serves a [`ToolServer`] over arbitrary streams with byte and flush policy.
///
/// # Errors
///
/// Returns stream failures or an invalid transport configuration.
pub fn serve_streams_with_config(
    server: &mut impl ToolServer,
    reader: impl BufRead,
    writer: impl Write,
    config: TransportConfig,
) -> io::Result<()> {
    transport::serve_streams_configured(server, reader, writer, config)
}

/// Processes one newline-free JSON-RPC message.
///
/// Returns `true` when a response was written and `false` for an empty line or
/// notification. This is useful for embedding mcport in an existing blocking
/// transport without giving the runtime ownership of its read loop.
///
/// # Errors
///
/// Returns only writer failures.
pub fn serve_message(
    server: &mut impl ToolServer,
    line: &str,
    writer: &mut impl Write,
) -> io::Result<bool> {
    let line = line.trim_start_matches('\u{feff}');
    if line.trim().is_empty() {
        return Ok(false);
    }
    fast::dispatch_line(server, line, writer)
}

/// Dispatches one parsed JSON-RPC request.
///
/// Returns `None` for notifications (requests without an `id`), which must
/// not be answered.
pub fn dispatch(server: &mut impl ToolServer, request: &Value) -> Option<Value> {
    if !request.is_object() {
        return Some(protocol::error(
            &Value::Null,
            -32_600,
            "invalid JSON-RPC request",
        ));
    }
    let id = request.get("id").cloned();
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(protocol::error(
            id.as_ref().unwrap_or(&Value::Null),
            -32_600,
            "invalid JSON-RPC version",
        ));
    }
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Some(protocol::error(
            id.as_ref().unwrap_or(&Value::Null),
            -32_600,
            "missing JSON-RPC method",
        ));
    };
    let modern = match request_uses_modern_protocol(request, id.as_ref().unwrap_or(&Value::Null)) {
        Ok(modern) => modern,
        Err(response) => return Some(response),
    };
    let id = id?;
    Some(match method {
        "initialize" => initialize_response(server, request, &id),
        "server/discover" if modern => discover_response(server, &id),
        "ping" if modern => protocol::error(&id, -32_601, "method not found: ping"),
        "ping" => protocol::success(&id, &json!({})),
        "tools/list" => tools_list_response(server, request, &id, modern),
        "tools/call" => tool_call_response(server, request, &id, modern),
        _ => protocol::error(&id, -32_601, format!("method not found: {method}")),
    })
}

fn request_uses_modern_protocol(request: &Value, id: &Value) -> Result<bool, Value> {
    let requested_version = request
        .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(Value::as_str);
    let Some(requested_version) = requested_version else {
        return Ok(false);
    };
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&requested_version) {
        return Err(protocol::error_with_data(
            id,
            -32_022,
            "Unsupported protocol version",
            json!({
                "supported": SUPPORTED_PROTOCOL_VERSIONS,
                "requested": requested_version
            }),
        ));
    }
    if requested_version != MODERN_PROTOCOL_VERSION {
        return Err(protocol::error(
            id,
            -32_600,
            "legacy protocol versions require the initialize lifecycle",
        ));
    }
    if request
        .pointer("/params/_meta/io.modelcontextprotocol~1clientCapabilities")
        .is_none()
    {
        return Err(protocol::error(
            id,
            -32_602,
            "missing io.modelcontextprotocol/clientCapabilities",
        ));
    }
    Ok(true)
}

fn initialize_response(server: &impl ToolServer, request: &Value, id: &Value) -> Value {
    let requested_version = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    let version = negotiate_protocol_version(requested_version);
    let identity = server.identity();
    protocol::success(
        id,
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

fn discover_response(server: &impl ToolServer, id: &Value) -> Value {
    let identity = server.identity();
    protocol::success(
        id,
        &json!({
            "resultType": "complete",
            "supportedVersions": SUPPORTED_PROTOCOL_VERSIONS,
            "capabilities": {"tools": {"listChanged": false}},
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": identity.name,
                    "version": identity.version
                }
            },
            "instructions": identity.instructions
        }),
    )
}

fn tool_call_response(
    server: &mut impl ToolServer,
    request: &Value,
    id: &Value,
    modern: bool,
) -> Value {
    let Some(name) = request.pointer("/params/name").and_then(Value::as_str) else {
        return protocol::error(id, -32_602, "tools/call requires params.name");
    };
    if server.has_tool(name) == Some(false) {
        return protocol::error(id, -32_602, format!("unknown tool: {name}"));
    }
    let arguments = request
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut response = match server.call(name, arguments) {
        ToolReply::Success { value, structured } => protocol::tool_success(id, &value, structured),
        ToolReply::Serialized { value, structured } => {
            protocol::tool_success_raw(id, &value, structured)
        }
        ToolReply::StructuredAny { value } if modern => protocol::tool_success_raw_any(id, &value),
        ToolReply::StructuredAny { value } => protocol::tool_success_raw(id, &value, false),
        ToolReply::Error(message) => protocol::tool_error(id, message),
    };
    if modern {
        protocol::mark_complete(&mut response);
    }
    response
}

fn tools_list_response(
    server: &mut impl ToolServer,
    request: &Value,
    id: &Value,
    modern: bool,
) -> Value {
    let cursor = request.pointer("/params/cursor").and_then(Value::as_str);
    let page = if server.catalog_is_paginated() || cursor.is_some() {
        match server.catalog_page(cursor) {
            Ok(page) => page,
            Err(message) => return protocol::error(id, -32_602, message),
        }
    } else {
        ToolPage::complete(server.catalog())
    };
    let mut result = json!({"tools": page.tools});
    if let (Some(result), Some(next_cursor)) = (result.as_object_mut(), page.next_cursor) {
        result.insert("nextCursor".to_owned(), Value::String(next_cursor));
    }
    let mut response = protocol::success(id, &result);
    if modern {
        protocol::mark_complete(&mut response);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::{
        dispatch, json, serve_message, serve_streams, ServerIdentity, ToolReply, ToolServer, Value,
    };

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
                "scalar" => ToolReply::structured(5),
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

        let unknown_version = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "initialize",
                "params": {"protocolVersion": "2099-01-01"}
            }),
        )
        .unwrap();
        assert_eq!(
            unknown_version["result"]["protocolVersion"],
            super::DEFAULT_PROTOCOL_VERSION
        );

        let listed = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
        )
        .unwrap();
        assert_eq!(listed["result"]["tools"].as_array().map(Vec::len), Some(1));

        let called = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 5,
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
                "id": 6,
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
                "id": 7,
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

        let wrong_version = dispatch(
            &mut server,
            &json!({"jsonrpc": "1.0", "id": 2, "method": "ping"}),
        )
        .unwrap();
        assert_eq!(wrong_version["error"]["code"], -32_600);

        let non_object = dispatch(&mut server, &json!([])).unwrap();
        assert_eq!(non_object["error"]["code"], -32_600);

        let unknown = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32_601);

        let unnamed = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call"}),
        )
        .unwrap();
        assert_eq!(unnamed["error"]["code"], -32_602);

        let notification = dispatch(
            &mut server,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        assert!(notification.is_none(), "notifications are not answered");

        let malformed_notification =
            dispatch(&mut server, &json!({"jsonrpc": "1.0", "method": "ping"})).unwrap();
        assert_eq!(malformed_notification["id"], Value::Null);
        assert_eq!(malformed_notification["error"]["code"], -32_600);
    }

    #[test]
    fn supports_modern_discovery_and_versioned_tool_results() {
        let modern_meta = json!({
            "io.modelcontextprotocol/protocolVersion": super::MODERN_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let mut server = Echo { calls: 0 };
        let discovered = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": "discover",
                "method": "server/discover",
                "params": {"_meta": modern_meta.clone()}
            }),
        )
        .unwrap();
        assert_eq!(discovered["result"]["resultType"], "complete");
        assert_eq!(
            discovered["result"]["supportedVersions"][0],
            super::MODERN_PROTOCOL_VERSION
        );
        assert_eq!(
            discovered["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "echo"
        );

        let listed = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {"_meta": modern_meta.clone()}
            }),
        )
        .unwrap();
        assert_eq!(listed["result"]["resultType"], "complete");

        let called = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "_meta": modern_meta.clone(),
                    "name": "echo",
                    "arguments": {"modern": true}
                }
            }),
        )
        .unwrap();
        assert_eq!(called["result"]["resultType"], "complete");
        assert_eq!(called["result"]["structuredContent"]["modern"], true);

        let scalar = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": "scalar",
                "method": "tools/call",
                "params": {
                    "_meta": modern_meta,
                    "name": "scalar",
                    "arguments": {}
                }
            }),
        )
        .unwrap();
        assert_eq!(scalar["result"]["structuredContent"], 5);

        let unsupported = dispatch(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/list",
                "params": {"_meta": {
                    "io.modelcontextprotocol/protocolVersion": "1900-01-01",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }}
            }),
        )
        .unwrap();
        assert_eq!(unsupported["error"]["code"], -32_022);
        assert_eq!(unsupported["error"]["data"]["requested"], "1900-01-01");
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

        let ping = blazingly_json::from_str::<Value>(lines[0]).unwrap();
        assert_eq!(ping["id"], 1, "BOM-prefixed first request still parses");

        let parse_error = blazingly_json::from_str::<Value>(lines[1]).unwrap();
        assert_eq!(parse_error["error"]["code"], -32_700);

        let called = blazingly_json::from_str::<Value>(lines[2]).unwrap();
        assert_eq!(called["result"]["structuredContent"]["ok"], true);
        assert_eq!(server.calls, 1);
    }

    #[test]
    fn fast_message_path_preserves_public_dispatch_semantics() {
        let fixtures = [
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            r#"{"jsonrpc":"2.0","id":"future","method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#,
            r#"{"method":"initialize","id":-2,"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"ok":true}}}"#,
            r#"{"params":{"arguments":[1,2],"name":"fl\u0061t"},"method":"tools/call","id":4,"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"missing"}}"#,
            r#"{"jsonrpc":"2.0","id":6}"#,
            r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#,
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call"}"#,
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":null}"#,
            r#"{"jsonrpc":"2.0","id":10,"method":false}"#,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"echo","arguments":{"text":"quote: \" slash: \\ newline: \n snowman: ☃"}}}"#,
            r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":{"legacy":true},"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"1.0","method":"ping"}"#,
            r#"{"jsonrpc":"2.0","method":false}"#,
            r"[]",
        ];

        for fixture in fixtures {
            let request = blazingly_json::from_str::<Value>(fixture).unwrap();
            let mut expected_server = Echo { calls: 0 };
            let expected = dispatch(&mut expected_server, &request);

            let mut actual_server = Echo { calls: 0 };
            let mut output = Vec::new();
            let wrote = serve_message(&mut actual_server, fixture, &mut output).unwrap();
            let actual = if output.is_empty() {
                None
            } else {
                Some(blazingly_json::from_slice::<Value>(&output).unwrap())
            };

            assert_eq!(wrote, actual.is_some(), "fixture: {fixture}");
            assert_eq!(actual, expected, "fixture: {fixture}");
            assert_eq!(actual_server.calls, expected_server.calls);
        }
    }
}
