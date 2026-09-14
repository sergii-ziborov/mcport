use mcport::{tool, tools, McpServer};

/// Adds two signed 32-bit integers.
#[tool]
fn add(left: i32, right: i32) -> i64 {
    i64::from(left) + i64::from(right)
}

fn main() -> std::io::Result<()> {
    McpServer::new("math", env!("CARGO_PKG_VERSION"))
        .tools(tools![add])
        .serve()
}
