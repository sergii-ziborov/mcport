use mcport::{
    json, ConcurrentMcpServer, FlushPolicy, McpServer, RuntimeConfig, ToolReply, TransportLimits,
};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    if std::env::args().any(|argument| argument == "--controlled") {
        controlled()
    } else {
        inline()
    }
}

fn inline() -> std::io::Result<()> {
    McpServer::new("mcport-test", "1.0.0")
        .tool(
            "echo",
            "Echoes arguments.",
            json!({"type": "object"}),
            ToolReply::structured,
        )
        .serve_with_limits(TransportLimits::new(1024, 512))
}

fn controlled() -> std::io::Result<()> {
    let config = RuntimeConfig {
        transport: TransportLimits::new(1024, 512),
        max_in_flight: 2,
        queue_depth: 4,
        output_queue_depth: 4,
        output_flush_policy: FlushPolicy::PerMessage,
        handler_deadline: Some(Duration::from_millis(100)),
    };
    ConcurrentMcpServer::new("mcport-controlled-test", "1.0.0")
        .tool(
            "wait",
            "Waits for cancellation.",
            json!({"type": "object"}),
            |context, _| {
                while !context.is_cancelled() {
                    std::thread::yield_now();
                }
                ToolReply::text("late")
            },
        )
        .tool("panic", "Panics.", json!({"type": "object"}), |_, _| {
            panic!("fixture panic")
        })
        .tool(
            "progress",
            "Reports progress.",
            json!({"type": "object"}),
            |context, _| {
                context
                    .report_progress(1.0, Some(2.0), Some("half"))
                    .expect("first progress");
                context
                    .report_progress(2.0, Some(2.0), Some("done"))
                    .expect("second progress");
                ToolReply::structured(json!({"done": true}))
            },
        )
        .tool(
            "large",
            "Returns an oversized result.",
            json!({"type": "object"}),
            |_, _| ToolReply::text("x".repeat(2048)),
        )
        .serve(config)
}
