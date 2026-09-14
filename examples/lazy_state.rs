use mcport::{tool, tools, McpServer};

struct Index {
    loaded: Option<Vec<String>>,
}

impl Index {
    const fn new() -> Self {
        Self { loaded: None }
    }

    fn search(&mut self, query: &str, limit: u32) -> Vec<String> {
        let entries = self.loaded.get_or_insert_with(load_index);
        entries
            .iter()
            .filter(|entry| entry.contains(query))
            .take(limit as usize)
            .cloned()
            .collect()
    }
}

fn load_index() -> Vec<String> {
    vec!["alpha".into(), "alphabet".into(), "beta".into()]
}

/// Finds matching symbols in the local index.
#[allow(clippy::needless_pass_by_value)]
#[tool(read_only)]
fn search(
    #[arg(description = "Text to find")] query: String,
    #[arg(default = 20, min = 1, max = 100)] limit: u32,
    #[state] index: &mut Index,
) -> Vec<String> {
    index.search(&query, limit)
}

fn main() -> std::io::Result<()> {
    McpServer::with_state("index", env!("CARGO_PKG_VERSION"), Index::new())
        .tools(tools![search])
        .serve()
}
