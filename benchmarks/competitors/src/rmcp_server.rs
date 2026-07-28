use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    transport::stdio,
    ErrorData, RoleServer, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};
use std::future::{ready, Future};

#[derive(Clone)]
struct BenchServer;

impl ServerHandler for BenchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some("Competitor benchmark server.".into()),
            ..Default::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"},
                "include_source": {"type": "boolean"}
            },
            "required": ["query", "limit", "include_source"]
        })
        .as_object()
        .expect("schema is an object")
        .clone();
        ready(Ok(ListToolsResult {
            tools: vec![Tool::new("query_graph", "Queries the graph.", schema)],
            ..Default::default()
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        let arguments = request.arguments.unwrap_or_default();
        let value = json!({
            "nodes": 12,
            "query": arguments.get("query").cloned().unwrap_or(Value::Null),
            "limit": arguments.get("limit").cloned().unwrap_or(Value::Null),
            "include_source": arguments.get("include_source").cloned().unwrap_or(Value::Null)
        });
        ready(Ok(CallToolResult {
            content: vec![Content::text(value.to_string())],
            structured_content: Some(value),
            is_error: Some(false),
            meta: None,
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = BenchServer.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
