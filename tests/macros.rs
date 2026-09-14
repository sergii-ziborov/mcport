#![cfg(feature = "macros")]

use mcport::{
    json, serve_message, tool, tools, IntoToolReply, Json, McpServer, ToolError, ToolOutput,
    ToolReply, ToolSchema, Value,
};
use serde::{Deserialize, Serialize};

/// Adds two integers.
#[tool]
fn add(left: i32, right: i32) -> i64 {
    i64::from(left) + i64::from(right)
}

/// Finds matching symbols.
#[allow(clippy::needless_pass_by_value)]
#[tool(read_only, closed_world)]
fn search(
    #[arg(description = "Text to find")] query: String,
    #[arg(default = 20, min = 1, max = 100)] limit: u32,
) -> Result<SearchResult, ToolError> {
    if query.is_empty() {
        return Err(ToolError::new("query must not be empty"));
    }
    Ok(SearchResult {
        matches: vec![format!("{query}:{limit}")],
    })
}

#[derive(Serialize, ToolOutput)]
struct SearchResult {
    matches: Vec<String>,
}

#[allow(clippy::needless_pass_by_value)]
#[tool]
fn greet(name: String) -> String {
    format!("hello {name}")
}

#[tool]
fn ping() -> &'static str {
    "pong"
}

fn serve_call(server: &mut McpServer, name: &str, arguments: &str) -> Value {
    let mut output = Vec::new();
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
    );
    serve_message(server, &line, &mut output).unwrap();
    parse_json(&output)
}

fn serve_list(server: &mut McpServer) -> Value {
    let mut output = Vec::new();
    serve_message(
        server,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        &mut output,
    )
    .unwrap();
    parse_json(&output)
}

fn parse_json(bytes: &[u8]) -> Value {
    let text = std::str::from_utf8(bytes).expect("utf8 JSON");
    mcport::__private::from_str(text).expect("valid JSON")
}

#[test]
fn generated_add_stays_a_plain_rust_function() {
    assert_eq!(add(2, 3), 5);
}

#[test]
fn generated_add_answers_over_mcp() {
    let mut server = McpServer::new("math", "1.0.0").tools(tools![add]);
    let listed = serve_list(&mut server);
    assert_eq!(listed["result"]["tools"][0]["name"], "add");
    assert_eq!(
        listed["result"]["tools"][0]["description"],
        "Adds two integers."
    );
    assert_eq!(
        listed["result"]["tools"][0]["inputSchema"]["required"],
        json!(["left", "right"])
    );

    let called = serve_call(&mut server, "add", r#"{"left":2,"right":3}"#);
    assert_eq!(called["result"]["structuredContent"]["result"], 5);
}

#[test]
fn defaults_apply_on_omission_but_null_is_rejected() {
    let mut server = McpServer::new("search", "1.0.0").tools(tools![search]);
    let omitted = serve_call(&mut server, "search", r#"{"query":"foo"}"#);
    assert_eq!(
        omitted["result"]["structuredContent"]["matches"][0],
        "foo:20"
    );

    let explicit_null = serve_call(&mut server, "search", r#"{"query":"foo","limit":null}"#);
    assert_eq!(explicit_null["result"]["isError"], true);

    let too_large = serve_call(&mut server, "search", r#"{"query":"foo","limit":101}"#);
    assert_eq!(too_large["result"]["isError"], true);
    assert!(too_large["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("limit"));

    let unknown = serve_call(&mut server, "search", r#"{"query":"foo","mystery":true}"#);
    assert_eq!(unknown["result"]["isError"], true);

    let catalog = serve_list(&mut server);
    assert_eq!(
        catalog["result"]["tools"][0]["annotations"]["readOnlyHint"],
        true
    );
    assert_eq!(
        catalog["result"]["tools"][0]["annotations"]["openWorldHint"],
        false
    );
    assert!(
        catalog["result"]["tools"][0]["inputSchema"]["properties"]["query"]["description"]
            .as_str()
            .unwrap()
            .contains("Text to find")
    );
}

#[test]
fn result_err_is_a_tool_failure() {
    let mut server = McpServer::new("search", "1.0.0").tools(tools![search]);
    let failed = serve_call(&mut server, "search", r#"{"query":""}"#);
    assert_eq!(failed["result"]["isError"], true);
    assert_eq!(
        failed["result"]["content"][0]["text"],
        "query must not be empty"
    );
    assert!(failed["result"].get("structuredContent").is_none());
}

#[test]
fn literal_text_has_no_json_quote_layer() {
    let mut server = McpServer::new("greeter", "1.0.0").tools(tools![greet, ping]);
    let greeted = serve_call(&mut server, "greet", r#"{"name":"Ada"}"#);
    assert_eq!(greeted["result"]["content"][0]["text"], "hello Ada");

    let pinged = serve_call(&mut server, "ping", "{}");
    assert_eq!(pinged["result"]["content"][0]["text"], "pong");

    let quoted = ToolReply::text("hello");
    let ToolReply::Serialized { value, .. } = quoted else {
        panic!("text() must keep JSON serialization");
    };
    assert_eq!(value.get(), r#""hello""#);
}

#[test]
fn json_wrapper_and_schema_derive_round_trip() {
    #[derive(Deserialize, Serialize, ToolSchema)]
    struct Payload {
        name: String,
        count: Option<u32>,
    }

    let schema = Payload::tool_schema();
    assert_eq!(schema["properties"]["name"]["type"], "string");
    assert_eq!(schema["required"], json!(["name"]));

    let reply = Json(Payload {
        name: "n".into(),
        count: None,
    })
    .into_tool_reply();
    assert!(matches!(reply, ToolReply::Serialized { .. }));
}

#[test]
fn duplicate_generated_names_refuse_to_serve() {
    let mut server = McpServer::new("math", "1.0.0")
        .tools(tools![add])
        .tools(tools![add]);
    let error = server
        .serve_streams(&b""[..], Vec::new())
        .expect_err("duplicate generated names must fail startup");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("add"));
}
