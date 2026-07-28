use crate::{
    negotiate_protocol_version, ServerIdentity, ToolReply, ToolServer, DEFAULT_PROTOCOL_VERSION,
};
use blazingly_json::{from_str, CanonicalScanner, JsonCursor, RawJson, RawValue, Value};
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
enum CanonicalControl<'a> {
    Ping(RequestId<'a>),
    Initialize {
        id: RequestId<'a>,
        protocol_version: &'a str,
    },
    ToolsList(RequestId<'a>),
}

#[derive(Clone, Copy)]
struct CanonicalToolCall<'a> {
    id: RequestId<'a>,
    name: &'a str,
    arguments: RawJson<'a>,
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
    capabilities: Capabilities,
    server_info: ServerInfo<'a>,
    instructions: &'a str,
}

#[derive(Serialize)]
struct Capabilities {
    tools: ToolsCapability,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolsCapability {
    list_changed: bool,
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
    content: [TextContent<'a>; 1],
    is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_content: Option<&'a Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SerializedToolResult<'a> {
    content: [TextContent<'a>; 1],
    is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_content: Option<&'a RawValue>,
}

#[derive(Serialize)]
struct TextContent<'a> {
    r#type: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct Empty {}

pub(crate) fn dispatch_line(
    server: &mut impl ToolServer,
    line: &str,
    writer: &mut impl Write,
) -> io::Result<bool> {
    if let Some(control) = recognize_control(line) {
        write_canonical_control(server, writer, control)?;
        return Ok(true);
    }
    if let Some(call) = recognize_tool_call(line) {
        write_tool_call(
            server,
            writer,
            call.id,
            Some(call.name),
            Some(call.arguments),
        )?;
        return Ok(true);
    }

    let Ok(route) = parse_route(line) else {
        return match from_str::<Value>(line) {
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
        };
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
        _ => write_error(writer, id, -32_601, format!("method not found: {method}"))?,
    }
    Ok(true)
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

fn recognize_control(line: &str) -> Option<CanonicalControl<'_>> {
    recognize_ping(line)
        .or_else(|| recognize_initialize(line))
        .or_else(|| recognize_tools_list(line))
}

fn recognize_ping(line: &str) -> Option<CanonicalControl<'_>> {
    recognize_id_and_suffix(line, r#","method":"ping"}"#).map(CanonicalControl::Ping)
}

fn recognize_tools_list(line: &str) -> Option<CanonicalControl<'_>> {
    recognize_id_and_suffix(line, r#","method":"tools/list"}"#).map(CanonicalControl::ToolsList)
}

fn recognize_tool_call(line: &str) -> Option<CanonicalToolCall<'_>> {
    let mut scanner = CanonicalScanner::new(line);
    scanner.literal(r#"{"jsonrpc":"2.0","id":"#)?;
    let id = if scanner.remaining().starts_with('"') {
        RequestId::String(scanner.plain_string()?)
    } else {
        RequestId::Unsigned(scanner.unsigned()?)
    };
    scanner.literal(r#","method":"tools/call","params":{"name":"#)?;
    let name = scanner.plain_string()?;
    scanner.literal(r#","arguments":"#)?;
    let arguments = scanner.remaining().strip_suffix("}}")?;
    let arguments = from_str::<RawJson<'_>>(arguments).ok()?;
    Some(CanonicalToolCall {
        id,
        name,
        arguments,
    })
}

fn recognize_initialize(line: &str) -> Option<CanonicalControl<'_>> {
    let suffix = r#","method":"initialize","params":{"protocolVersion":"#;

    let mut scanner = CanonicalScanner::new(line);
    if scanner.literal(r#"{"jsonrpc":"2.0","id":"#).is_some()
        && scanner.remaining().starts_with('"')
    {
        let id = scanner.plain_string()?;
        scanner.literal(suffix)?;
        let protocol_version = scanner.plain_string()?;
        scanner.literal("}}")?;
        if scanner.is_finished() {
            return Some(CanonicalControl::Initialize {
                id: RequestId::String(id),
                protocol_version,
            });
        }
    }

    let mut scanner = CanonicalScanner::new(line);
    scanner.literal(r#"{"jsonrpc":"2.0","id":"#)?;
    let id = scanner.unsigned()?;
    scanner.literal(suffix)?;
    let protocol_version = scanner.plain_string()?;
    scanner.literal("}}")?;
    scanner
        .is_finished()
        .then_some(CanonicalControl::Initialize {
            id: RequestId::Unsigned(id),
            protocol_version,
        })
}

fn recognize_id_and_suffix<'a>(line: &'a str, suffix: &str) -> Option<RequestId<'a>> {
    let mut scanner = CanonicalScanner::new(line);
    if scanner.literal(r#"{"jsonrpc":"2.0","id":"#).is_some()
        && scanner.remaining().starts_with('"')
    {
        let id = scanner.plain_string()?;
        scanner.literal(suffix)?;
        if scanner.is_finished() {
            return Some(RequestId::String(id));
        }
    }

    let mut scanner = CanonicalScanner::new(line);
    scanner.literal(r#"{"jsonrpc":"2.0","id":"#)?;
    let id = scanner.unsigned()?;
    scanner.literal(suffix)?;
    scanner.is_finished().then_some(RequestId::Unsigned(id))
}

fn write_canonical_control(
    server: &mut impl ToolServer,
    writer: &mut impl Write,
    control: CanonicalControl<'_>,
) -> io::Result<()> {
    match control {
        CanonicalControl::Ping(id) => write_empty(writer, id),
        CanonicalControl::Initialize {
            id,
            protocol_version,
        } => write_initialize(server, writer, id, protocol_version),
        CanonicalControl::ToolsList(id) => write_tools_list(server, writer, id),
    }
}

fn write_empty(writer: &mut impl Write, id: RequestId<'_>) -> io::Result<()> {
    write(
        writer,
        &Response {
            jsonrpc: "2.0",
            id,
            result: Empty {},
        },
    )
}

fn write_initialize(
    server: &impl ToolServer,
    writer: &mut impl Write,
    id: RequestId<'_>,
    protocol_version: &str,
) -> io::Result<()> {
    if let Some(identity) = server.identity_ref() {
        return write_initialize_with_identity(writer, id, protocol_version, identity);
    }
    let identity = server.identity();
    write_initialize_with_identity(writer, id, protocol_version, &identity)
}

fn write_initialize_with_identity(
    writer: &mut impl Write,
    id: RequestId<'_>,
    protocol_version: &str,
    identity: &ServerIdentity,
) -> io::Result<()> {
    let protocol_version = negotiate_protocol_version(Some(protocol_version));
    write(
        writer,
        &Response {
            jsonrpc: "2.0",
            id,
            result: InitializeResult {
                protocol_version,
                capabilities: Capabilities {
                    tools: ToolsCapability {
                        list_changed: false,
                    },
                },
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
    if let Some(catalog) = server.catalog_ref() {
        return write_tool_list(writer, id, catalog);
    }
    let catalog = server.catalog();
    write_tool_list(writer, id, &catalog)
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
        ToolReply::Success { value, structured } => {
            let structured = structured && value.is_object();
            let text = if structured {
                blazingly_json::to_string_pretty(&value)
            } else {
                blazingly_json::to_string(&value)
            }
            .unwrap_or_else(|_| "{}".to_owned());
            write(
                writer,
                &Response {
                    jsonrpc: "2.0",
                    id,
                    result: ToolResult {
                        content: [TextContent {
                            r#type: "text",
                            text: &text,
                        }],
                        is_error: false,
                        structured_content: structured.then_some(&value),
                    },
                },
            )
        }
        ToolReply::Serialized { value, structured } => write(
            writer,
            &Response {
                jsonrpc: "2.0",
                id,
                result: SerializedToolResult {
                    content: [TextContent {
                        r#type: "text",
                        text: value.get(),
                    }],
                    is_error: false,
                    structured_content: structured.then_some(value.as_ref()),
                },
            },
        ),
        ToolReply::Error(message) => write(
            writer,
            &Response {
                jsonrpc: "2.0",
                id,
                result: ToolResult {
                    content: [TextContent {
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

#[cfg(test)]
mod tests {
    use super::{parse_route, recognize_control, recognize_tool_call, CanonicalControl, RequestId};

    #[test]
    fn recognizes_compact_control_messages_and_rejects_near_misses() {
        assert!(matches!(
            recognize_control(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#),
            Some(CanonicalControl::Ping(RequestId::Unsigned(7)))
        ));
        assert!(matches!(
            recognize_control(
                r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#
            ),
            Some(CanonicalControl::Initialize {
                protocol_version: "2025-06-18",
                ..
            })
        ));
        assert!(recognize_control(r#"{"id":7,"jsonrpc":"2.0","method":"ping"}"#).is_none());
        assert!(recognize_control(r#"{"jsonrpc": "2.0","id":7,"method":"ping"}"#).is_none());
    }

    #[test]
    fn recognizes_only_complete_canonical_tool_calls() {
        let call = recognize_tool_call(
            r#"{"jsonrpc":"2.0","id":"a","method":"tools/call","params":{"name":"echo","arguments":{"ok":true}}}"#,
        )
        .unwrap();
        assert!(matches!(call.id, RequestId::String("a")));
        assert_eq!(call.name, "echo");
        assert_eq!(call.arguments.get(), r#"{"ok":true}"#);

        assert!(recognize_tool_call(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","arguments":[1,2]}}"#,
        )
        .is_some());
        assert!(recognize_tool_call(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"arguments":{},"name":"echo"}}"#,
        )
        .is_none());
        assert!(recognize_tool_call(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"ec\u0068o","arguments":{}}}"#,
        )
        .is_none());
        assert!(recognize_tool_call(
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
