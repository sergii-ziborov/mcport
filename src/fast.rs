use crate::{
    negotiate_protocol_version, ServerIdentity, ToolReply, ToolServer, DEFAULT_PROTOCOL_VERSION,
};
use blazingly_json::{from_str, CanonicalScanner, JsonCursor, RawJson, Value};
use serde::de::{Deserialize, Deserializer, Visitor};
use serde::Serialize;
use std::borrow::Cow;
use std::fmt;
use std::io::{self, Write};
use std::marker::PhantomData;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(untagged)]
enum RequestId<'a> {
    String(&'a str),
    Unsigned(u64),
    Signed(i64),
    Null,
}

struct RequestIdVisitor<'a>(PhantomData<&'a str>);

impl<'de: 'a, 'a> Visitor<'de> for RequestIdVisitor<'a> {
    type Value = RequestId<'a>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON-RPC string, integer, or null id")
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(RequestId::String(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(RequestId::Unsigned(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(RequestId::Signed(value))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(RequestId::Null)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for RequestId<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(RequestIdVisitor(PhantomData))
    }
}

#[derive(Debug, Default)]
struct LooseString<'a>(Option<Cow<'a, str>>);

struct LooseStringVisitor<'a>(PhantomData<&'a str>);

impl<'de: 'a, 'a> Visitor<'de> for LooseStringVisitor<'a> {
    type Value = LooseString<'a>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(LooseString(Some(Cow::Borrowed(value))))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(LooseString(Some(Cow::Owned(value.to_owned()))))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(LooseString(Some(Cow::Owned(value))))
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(LooseString(None))
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(LooseString(None))
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(LooseString(None))
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(LooseString(None))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(LooseString(None))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for LooseString<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(LooseStringVisitor(PhantomData))
    }
}

#[derive(Default)]
struct Route<'a> {
    jsonrpc: Option<Cow<'a, str>>,
    id: Option<RequestId<'a>>,
    method: Option<Cow<'a, str>>,
    protocol_version: Option<Cow<'a, str>>,
    name: Option<Cow<'a, str>>,
    arguments: Option<RawJson<'a>>,
}

#[derive(Clone, Copy)]
enum CanonicalRequest<'a> {
    Ping(RequestId<'a>),
    Initialize {
        id: RequestId<'a>,
        protocol_version: &'a str,
    },
    ToolsList(RequestId<'a>),
    ToolCall {
        id: RequestId<'a>,
        name: &'a str,
        arguments: RawJson<'a>,
    },
}

#[derive(Serialize)]
struct Response<'a, T> {
    jsonrpc: &'static str,
    id: RequestId<'a>,
    result: T,
}

#[derive(Serialize)]
struct ErrorResponse<'a> {
    jsonrpc: &'static str,
    id: RequestId<'a>,
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: i64,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult<'a> {
    protocol_version: &'a str,
    capabilities: &'a Value,
    server_info: ServerInfo<'a>,
    instructions: &'a str,
}

#[derive(Serialize)]
struct ServerInfo<'a> {
    name: &'a str,
    version: &'a str,
}

#[derive(Serialize)]
struct ToolList<'a> {
    tools: &'a Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolResult<'a> {
    content: &'a [TextContent<'a>],
    is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_content: Option<&'a Value>,
}

#[derive(Serialize)]
struct TextContent<'a> {
    r#type: &'static str,
    text: &'a str,
}

pub(crate) fn dispatch_line(
    server: &mut impl ToolServer,
    line: &str,
    writer: &mut impl Write,
) -> io::Result<bool> {
    if let Some(request) = recognize_canonical(line) {
        write_canonical_request(server, writer, request)?;
        return Ok(true);
    }
    if line.contains("io.modelcontextprotocol") || line.contains(r#""cursor""#) {
        return dispatch_owned_line(server, line, writer);
    }

    let Ok(route) = parse_route(line) else {
        return dispatch_owned_line(server, line, writer);
    };
    if route.jsonrpc.as_deref() != Some("2.0") {
        write_error(
            writer,
            route.id.unwrap_or(RequestId::Null),
            -32_600,
            "invalid JSON-RPC version",
        )?;
        return Ok(true);
    }
    let Some(method) = route.method.as_deref() else {
        write_error(
            writer,
            route.id.unwrap_or(RequestId::Null),
            -32_600,
            "missing JSON-RPC method",
        )?;
        return Ok(true);
    };
    let Some(id) = route.id else {
        return Ok(false);
    };

    match method {
        "initialize" => write_initialize(
            server,
            writer,
            id,
            route
                .protocol_version
                .as_deref()
                .unwrap_or(DEFAULT_PROTOCOL_VERSION),
        )?,
        "ping" => write_empty(writer, id)?,
        "tools/list" => write_tools_list(server, writer, id)?,
        "tools/call" => {
            write_tool_call(server, writer, id, route.name.as_deref(), route.arguments)?;
        }
        _ => return dispatch_owned_line(server, line, writer),
    }
    Ok(true)
}

#[cold]
#[inline(never)]
fn dispatch_owned_line(
    server: &mut impl ToolServer,
    line: &str,
    writer: &mut impl Write,
) -> io::Result<bool> {
    match from_str::<Value>(line) {
        Ok(request) => match crate::dispatch(server, &request) {
            Some(response) => {
                write(writer, &response)?;
                Ok(true)
            }
            None => Ok(false),
        },
        Err(error) => {
            write_error(writer, RequestId::Null, -32_700, error.to_string())?;
            Ok(true)
        }
    }
}

fn parse_route(line: &str) -> blazingly_json::Result<Route<'_>> {
    let mut route = Route::default();
    let mut cursor = JsonCursor::from_str(line);
    cursor.object(|request| {
        while let Some(field) = request.next_field()? {
            match field.name() {
                "jsonrpc" => route.jsonrpc = field.deserialize::<LooseString<'_>>()?.0,
                "id" => route.id = Some(field.deserialize::<RequestId<'_>>()?),
                "method" => route.method = field.deserialize::<LooseString<'_>>()?.0,
                "params" => field.object(|params| {
                    let mut protocol_version = None;
                    let mut name = None;
                    let mut arguments = None;
                    while let Some(field) = params.next_field()? {
                        match field.name() {
                            "protocolVersion" => {
                                protocol_version = field.deserialize::<LooseString<'_>>()?.0;
                            }
                            "name" => name = field.deserialize::<LooseString<'_>>()?.0,
                            "arguments" => arguments = Some(field.raw()?),
                            _ => field.skip()?,
                        }
                    }
                    route.protocol_version = protocol_version;
                    route.name = name;
                    route.arguments = arguments;
                    Ok(())
                })?,
                _ => field.skip()?,
            }
        }
        Ok(())
    })?;
    cursor.end()?;
    Ok(route)
}

fn recognize_canonical(line: &str) -> Option<CanonicalRequest<'_>> {
    let mut scanner = CanonicalScanner::new(line);
    scanner.literal(r#"{"jsonrpc":"2.0","id":"#)?;
    let id = if scanner.remaining().starts_with('"') {
        RequestId::String(scanner.plain_string()?)
    } else {
        RequestId::Unsigned(scanner.unsigned()?)
    };
    match scanner.remaining() {
        r#","method":"ping"}"# => Some(CanonicalRequest::Ping(id)),
        r#","method":"tools/list"}"# => Some(CanonicalRequest::ToolsList(id)),
        remaining
            if remaining.starts_with(r#","method":"initialize","params":{"protocolVersion":"#) =>
        {
            scanner.literal(r#","method":"initialize","params":{"protocolVersion":"#)?;
            let protocol_version = scanner.plain_string()?;
            scanner.literal("}}")?;
            scanner
                .is_finished()
                .then_some(CanonicalRequest::Initialize {
                    id,
                    protocol_version,
                })
        }
        remaining if remaining.starts_with(r#","method":"tools/call","params":{"name":"#) => {
            scanner.literal(r#","method":"tools/call","params":{"name":"#)?;
            let name = scanner.plain_string()?;
            scanner.literal(r#","arguments":"#)?;
            let arguments = scanner.remaining().strip_suffix("}}")?;
            let arguments = from_str::<RawJson<'_>>(arguments).ok()?;
            Some(CanonicalRequest::ToolCall {
                id,
                name,
                arguments,
            })
        }
        _ => None,
    }
}

fn write_canonical_request(
    server: &mut impl ToolServer,
    writer: &mut impl Write,
    request: CanonicalRequest<'_>,
) -> io::Result<()> {
    match request {
        CanonicalRequest::Ping(id) => write_empty(writer, id),
        CanonicalRequest::Initialize {
            id,
            protocol_version,
        } => write_initialize(server, writer, id, protocol_version),
        CanonicalRequest::ToolsList(id) => write_tools_list(server, writer, id),
        CanonicalRequest::ToolCall {
            id,
            name,
            arguments,
        } => write_tool_call(server, writer, id, Some(name), Some(arguments)),
    }
}

fn write_empty(writer: &mut impl Write, id: RequestId<'_>) -> io::Result<()> {
    write_response_start(writer, id)?;
    finish_response(writer, b"{}")
}

fn write_initialize(
    server: &impl ToolServer,
    writer: &mut impl Write,
    id: RequestId<'_>,
    protocol_version: &str,
) -> io::Result<()> {
    let capabilities = server
        .capabilities_ref()
        .cloned()
        .unwrap_or_else(|| server.capabilities());
    if let Some(identity) = server.identity_ref() {
        return write_initialize_with_identity(
            writer,
            id,
            protocol_version,
            identity,
            &capabilities,
        );
    }
    let identity = server.identity();
    write_initialize_with_identity(writer, id, protocol_version, &identity, &capabilities)
}

fn write_initialize_with_identity(
    writer: &mut impl Write,
    id: RequestId<'_>,
    protocol_version: &str,
    identity: &ServerIdentity,
    capabilities: &Value,
) -> io::Result<()> {
    let protocol_version = negotiate_protocol_version(Some(protocol_version));
    write(
        writer,
        &Response {
            jsonrpc: "2.0",
            id,
            result: InitializeResult {
                protocol_version,
                capabilities,
                server_info: ServerInfo {
                    name: &identity.name,
                    version: &identity.version,
                },
                instructions: &identity.instructions,
            },
        },
    )
}

fn write_tools_list(
    server: &mut impl ToolServer,
    writer: &mut impl Write,
    id: RequestId<'_>,
) -> io::Result<()> {
    if server.catalog_is_paginated() {
        return write_first_tool_page(server, writer, id);
    }
    if let Some(catalog) = server.catalog_raw_ref() {
        write_response_start(writer, id)?;
        writer.write_all(br#"{"tools":"#)?;
        writer.write_all(catalog.get().as_bytes())?;
        return finish_response(writer, b"}");
    }
    if let Some(catalog) = server.catalog_ref() {
        return write_tool_list(writer, id, catalog);
    }
    let catalog = server.catalog();
    write_tool_list(writer, id, &catalog)
}

#[cold]
#[inline(never)]
fn write_first_tool_page(
    server: &mut impl ToolServer,
    writer: &mut impl Write,
    id: RequestId<'_>,
) -> io::Result<()> {
    match server.catalog_page(None) {
        Ok(page) => write_tool_page(writer, id, &page),
        Err(message) => write_error(writer, id, -32_602, message),
    }
}

fn write_tool_page(
    writer: &mut impl Write,
    id: RequestId<'_>,
    page: &crate::ToolPage,
) -> io::Result<()> {
    let mut result = blazingly_json::Map::new();
    result.insert("tools".to_owned(), page.tools.clone());
    if let Some(next_cursor) = &page.next_cursor {
        result.insert("nextCursor".to_owned(), Value::String(next_cursor.clone()));
    }
    write(
        writer,
        &Response {
            jsonrpc: "2.0",
            id,
            result: Value::Object(result),
        },
    )
}

fn write_tool_list(writer: &mut impl Write, id: RequestId<'_>, catalog: &Value) -> io::Result<()> {
    write(
        writer,
        &Response {
            jsonrpc: "2.0",
            id,
            result: ToolList { tools: catalog },
        },
    )
}

fn write_tool_call(
    server: &mut impl ToolServer,
    writer: &mut impl Write,
    id: RequestId<'_>,
    name: Option<&str>,
    arguments: Option<RawJson<'_>>,
) -> io::Result<()> {
    let Some(name) = name else {
        return write_error(writer, id, -32_602, "tools/call requires params.name");
    };
    if server.has_tool(name) == Some(false) {
        return write_error(writer, id, -32_602, format!("unknown tool: {name}"));
    }
    let arguments = match arguments {
        Some(arguments) => arguments,
        None => from_str::<RawJson<'_>>("{}").expect("empty object is valid JSON"),
    };
    match server.call_raw(name, arguments) {
        ToolReply::Success { value, payload } => {
            let structured = payload.is_structured() && value.is_object();
            let text = (payload.has_text() || !structured).then(|| {
                if structured {
                    blazingly_json::to_string_pretty(&value)
                } else {
                    blazingly_json::to_string(&value)
                }
                .unwrap_or_else(|_| "{}".to_owned())
            });
            let content = text.as_deref().map(|text| TextContent {
                r#type: "text",
                text,
            });
            write(
                writer,
                &Response {
                    jsonrpc: "2.0",
                    id,
                    result: ToolResult {
                        content: content.as_slice(),
                        is_error: false,
                        structured_content: structured.then_some(&value),
                    },
                },
            )
        }
        ToolReply::Serialized { value, payload } => {
            let structured = payload.is_structured() && value.get().starts_with('{');
            write_response_start(writer, id)?;
            if payload.has_text() || !structured {
                writer.write_all(br#"{"content":[{"type":"text","text":"#)?;
                blazingly_json::to_writer(&mut *writer, &value.get())?;
                writer.write_all(br#"}],"isError":false"#)?;
            } else {
                // The text block is the whole payload a second time. A client
                // that reads structured output has already been given it.
                writer.write_all(br#"{"content":[],"isError":false"#)?;
            }
            if structured {
                writer.write_all(br#","structuredContent":"#)?;
                writer.write_all(value.get().as_bytes())?;
            }
            finish_response(writer, b"}")
        }
        ToolReply::StructuredAny { value } => {
            write_response_start(writer, id)?;
            writer.write_all(br#"{"content":[{"type":"text","text":"#)?;
            blazingly_json::to_writer(&mut *writer, &value.get())?;
            writer.write_all(br#"}],"isError":false"#)?;
            finish_response(writer, b"}")
        }
        ToolReply::Error(message) => write(
            writer,
            &Response {
                jsonrpc: "2.0",
                id,
                result: ToolResult {
                    content: &[TextContent {
                        r#type: "text",
                        text: &message,
                    }],
                    is_error: true,
                    structured_content: None,
                },
            },
        ),
    }
}

fn write_error(
    writer: &mut impl Write,
    id: RequestId<'_>,
    code: i64,
    message: impl Into<String>,
) -> io::Result<()> {
    write(
        writer,
        &ErrorResponse {
            jsonrpc: "2.0",
            id,
            error: ErrorBody {
                code,
                message: message.into(),
            },
        },
    )
}

fn write(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    blazingly_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn write_response_start(writer: &mut impl Write, id: RequestId<'_>) -> io::Result<()> {
    writer.write_all(br#"{"jsonrpc":"2.0","id":"#)?;
    blazingly_json::to_writer(&mut *writer, &id)?;
    writer.write_all(br#","result":"#)
}

fn finish_response(writer: &mut impl Write, result_suffix: &[u8]) -> io::Result<()> {
    writer.write_all(result_suffix)?;
    writer.write_all(b"}\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::{parse_route, recognize_canonical, CanonicalRequest, RequestId};

    #[test]
    fn recognizes_compact_control_messages_and_rejects_near_misses() {
        assert!(matches!(
            recognize_canonical(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#),
            Some(CanonicalRequest::Ping(RequestId::Unsigned(7)))
        ));
        assert!(matches!(
            recognize_canonical(
                r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#
            ),
            Some(CanonicalRequest::Initialize {
                protocol_version: "2025-06-18",
                ..
            })
        ));
        assert!(recognize_canonical(r#"{"id":7,"jsonrpc":"2.0","method":"ping"}"#).is_none());
        assert!(recognize_canonical(r#"{"jsonrpc": "2.0","id":7,"method":"ping"}"#).is_none());
    }

    #[test]
    fn recognizes_only_complete_canonical_tool_calls() {
        let call = recognize_canonical(
            r#"{"jsonrpc":"2.0","id":"a","method":"tools/call","params":{"name":"echo","arguments":{"ok":true}}}"#,
        )
        .unwrap();
        let CanonicalRequest::ToolCall {
            id,
            name,
            arguments,
        } = call
        else {
            panic!("expected canonical tool call");
        };
        assert!(matches!(id, RequestId::String("a")));
        assert_eq!(name, "echo");
        assert_eq!(arguments.get(), r#"{"ok":true}"#);

        assert!(matches!(
            recognize_canonical(
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","arguments":[1,2]}}"#,
            ),
            Some(CanonicalRequest::ToolCall { .. })
        ));
        assert!(recognize_canonical(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"arguments":{},"name":"echo"}}"#,
        )
        .is_none());
        assert!(recognize_canonical(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"ec\u0068o","arguments":{}}}"#,
        )
        .is_none());
        assert!(recognize_canonical(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","arguments":{"unterminated":}}"#,
        )
        .is_none());
    }

    #[test]
    fn general_route_accepts_reordered_and_escaped_fields() {
        let route = parse_route(
            r#"{"params":{"arguments":{"ok":true},"name":"ec\u0068o"},"method":"tools/call","id":-7,"jsonrpc":"2.0"}"#,
        )
        .unwrap();
        assert!(matches!(route.id, Some(RequestId::Signed(-7))));
        assert_eq!(route.method.as_deref(), Some("tools/call"));
        assert_eq!(route.name.as_deref(), Some("echo"));
        assert_eq!(
            route.arguments.map(blazingly_json::RawJson::get),
            Some(r#"{"ok":true}"#)
        );
    }
}
