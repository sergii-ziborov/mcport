use mcport::{json, McpServer, ToolReply};
use serde::Deserialize;

#[derive(Deserialize)]
struct QueryArguments {
    query: String,
    limit: u64,
    include_source: bool,
}

fn main() -> std::io::Result<()> {
    McpServer::new("mcport-bench", "0.1.0")
        .typed_tool(
            "query_graph",
            "Queries the graph.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"},
                    "include_source": {"type": "boolean"}
                },
                "required": ["query", "limit", "include_source"]
            }),
            |arguments: QueryArguments| {
                ToolReply::structured(json!({
                    "nodes": 12,
                    "query": arguments.query,
                    "limit": arguments.limit,
                    "include_source": arguments.include_source
                }))
            },
        )
        .serve()
}
