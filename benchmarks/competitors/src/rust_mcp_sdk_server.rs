use async_trait::async_trait;
use rust_mcp_sdk::{
    error::SdkResult,
    macros,
    mcp_server::{server_runtime, McpServerOptions, ServerHandler},
    schema::*,
    *,
};
use serde_json::{json, Value};

#[macros::mcp_tool(name = "query_graph", description = "Queries the graph.")]
#[derive(Debug, serde::Deserialize, serde::Serialize, macros::JsonSchema)]
struct QueryGraphTool {
    query: String,
    limit: u64,
    include_source: bool,
}

#[derive(Default)]
struct BenchHandler;

#[async_trait]
impl ServerHandler for BenchHandler {
    async fn handle_list_tools_request(
        &self,
        _request: Option<PaginatedRequestParams>,
        _runtime: std::sync::Arc<dyn McpServer>,
    ) -> std::result::Result<ListToolsResult, RpcError> {
        Ok(ListToolsResult {
            tools: vec![QueryGraphTool::tool()],
            meta: None,
            next_cursor: None,
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: std::sync::Arc<dyn McpServer>,
    ) -> std::result::Result<CallToolResult, CallToolError> {
        let arguments = params.arguments.unwrap_or_default();
        let value = json!({
            "nodes": 12,
            "query": arguments.get("query").cloned().unwrap_or(Value::Null),
            "limit": arguments.get("limit").cloned().unwrap_or(Value::Null),
            "include_source": arguments.get("include_source").cloned().unwrap_or(Value::Null)
        });
        let structured = value.as_object().expect("tool result is an object").clone();
        Ok(CallToolResult::text_content(vec![value.to_string().into()])
            .with_structured_content(structured))
    }
}

#[tokio::main]
async fn main() -> SdkResult<()> {
    let server_details = InitializeResult {
        server_info: Implementation {
            name: "rust-mcp-sdk-bench".into(),
            version: "1.0.1".into(),
            title: Some("Competitor benchmark server".into()),
            description: None,
            icons: vec![],
            website_url: None,
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        protocol_version: ProtocolVersion::V2025_11_25.into(),
        instructions: Some("Competitor benchmark server.".into()),
        meta: None,
    };
    let transport = StdioTransport::new(TransportOptions::default())?;
    let handler = BenchHandler.to_mcp_server_handler();
    let server = server_runtime::create_server(McpServerOptions {
        transport,
        handler,
        server_details,
        task_store: None,
        client_task_store: None,
        message_observer: None,
    });
    server.start().await
}
