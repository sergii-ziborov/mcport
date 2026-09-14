use mcport::{tool, tools, McpServer};
use std::env;
use std::io::{self, Write};

/// Greets a person by name.
#[allow(clippy::needless_pass_by_value)]
#[tool]
fn greet(name: String) -> String {
    format!("hello {name}")
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("--mcp") => McpServer::new("greeter", env!("CARGO_PKG_VERSION"))
            .tools(tools![greet])
            .serve(),
        Some("greet") => {
            let name = args.next().unwrap_or_else(|| "world".to_owned());
            println!("{}", greet(name));
            Ok(())
        }
        _ => {
            let mut stderr = io::stderr();
            writeln!(
                stderr,
                "usage: embed_cli greet <name>\n       embed_cli --mcp"
            )?;
            Ok(())
        }
    }
}
