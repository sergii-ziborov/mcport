use mcport::{
    dispatch, json, serve_message, McpServer, ServerIdentity, ToolReply, ToolServer, Value,
};
use serde::Deserialize;
use std::hint::black_box;
use std::io::Write;
use std::time::{Duration, Instant};

const PING: &str = r#"{"jsonrpc":"2.0","id":17,"method":"ping"}"#;
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
const TOOL_LIST: &str = r#"{"jsonrpc":"2.0","id":18,"method":"tools/list"}"#;
const TOOL_CALL: &str = r#"{"jsonrpc":"2.0","id":"req-7","method":"tools/call","params":{"name":"query_graph","arguments":{"query":"entry points","limit":20,"include_source":true}}}"#;

struct Demo;

#[derive(Deserialize)]
struct QueryArguments {
    query: String,
    limit: u64,
    include_source: bool,
}

impl ToolServer for Demo {
    fn identity(&self) -> ServerIdentity {
        ServerIdentity::new("bench", "1.0.0", "Benchmark server.")
    }

    fn catalog(&mut self) -> Value {
        json!([{
            "name": "query_graph",
            "description": "Queries the graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"},
                    "include_source": {"type": "boolean"}
                }
            }
        }])
    }

    fn call(&mut self, name: &str, arguments: Value) -> ToolReply {
        if name == "query_graph" {
            ToolReply::structured(json!({
                "nodes": 12,
                "query": arguments["query"].clone(),
                "limit": arguments["limit"].clone(),
                "include_source": arguments["include_source"].clone()
            }))
        } else {
            ToolReply::error(format!("unknown tool: {name}"))
        }
    }
}

fn direct(server: &mut Demo, input: &str, output: &mut Vec<u8>) {
    output.clear();
    serve_message(server, input, output).unwrap();
    black_box(output.as_slice());
}

fn owned(server: &mut Demo, input: &str, output: &mut Vec<u8>) {
    output.clear();
    let request = blazingly_json::from_str::<Value>(input).unwrap();
    if let Some(response) = dispatch(server, &request) {
        blazingly_json::to_writer(&mut *output, &response).unwrap();
        output.write_all(b"\n").unwrap();
    }
    black_box(output.as_slice());
}

fn batch(iterations: u32, task: &mut impl FnMut()) -> Duration {
    let started = Instant::now();
    for _ in 0..iterations {
        task();
    }
    started.elapsed()
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn compare(name: &str, input: &str) {
    const ITERATIONS: u32 = 25_000;
    const ROUNDS: u32 = 24;

    let mut expected_server = Demo;
    let mut expected = Vec::new();
    owned(&mut expected_server, input, &mut expected);
    let mut actual_server = Demo;
    let mut actual = Vec::new();
    direct(&mut actual_server, input, &mut actual);
    let expected = blazingly_json::from_slice::<Value>(&expected).unwrap();
    let actual = blazingly_json::from_slice::<Value>(&actual).unwrap();
    assert_eq!(actual, expected);

    let mut direct_samples = Vec::with_capacity(ROUNDS as usize);
    let mut owned_samples = Vec::with_capacity(ROUNDS as usize);
    let mut direct_server = Demo;
    let mut owned_server = Demo;
    let mut direct_output = Vec::with_capacity(1_024);
    let mut owned_output = Vec::with_capacity(1_024);
    for round in 0..ROUNDS {
        let mut direct_task = || direct(&mut direct_server, black_box(input), &mut direct_output);
        let mut owned_task = || owned(&mut owned_server, black_box(input), &mut owned_output);
        let (direct_time, owned_time) = if round % 2 == 0 {
            (
                batch(ITERATIONS, &mut direct_task),
                batch(ITERATIONS, &mut owned_task),
            )
        } else {
            let owned_time = batch(ITERATIONS, &mut owned_task);
            let direct_time = batch(ITERATIONS, &mut direct_task);
            (direct_time, owned_time)
        };
        direct_samples.push(direct_time.as_secs_f64() * 1e9 / f64::from(ITERATIONS));
        owned_samples.push(owned_time.as_secs_f64() * 1e9 / f64::from(ITERATIONS));
    }

    let direct = median(&mut direct_samples);
    let owned = median(&mut owned_samples);
    println!(
        "{name:<12} direct={direct:>9.2} ns owned={owned:>9.2} ns speedup={:>5.2}x",
        owned / direct
    );
}

fn compare_typed_tool() {
    const ITERATIONS: u32 = 25_000;
    const ROUNDS: u32 = 24;

    let mut typed = McpServer::new("bench", "1.0.0").typed_tool(
        "query_graph",
        "Queries the graph.",
        json!({"type": "object"}),
        |arguments: QueryArguments| {
            ToolReply::structured(json!({
                "nodes": 12,
                "query": arguments.query,
                "limit": arguments.limit,
                "include_source": arguments.include_source
            }))
        },
    );
    let mut legacy = Demo;
    let mut typed_output = Vec::with_capacity(1_024);
    let mut legacy_output = Vec::with_capacity(1_024);
    serve_message(&mut typed, TOOL_CALL, &mut typed_output).unwrap();
    direct(&mut legacy, TOOL_CALL, &mut legacy_output);
    assert_eq!(
        blazingly_json::from_slice::<Value>(&typed_output).unwrap(),
        blazingly_json::from_slice::<Value>(&legacy_output).unwrap()
    );
    let mut typed_samples = Vec::with_capacity(ROUNDS as usize);
    let mut legacy_samples = Vec::with_capacity(ROUNDS as usize);
    for round in 0..ROUNDS {
        let mut typed_task = || {
            typed_output.clear();
            serve_message(&mut typed, black_box(TOOL_CALL), &mut typed_output).unwrap();
            black_box(typed_output.as_slice());
        };
        let mut legacy_task = || direct(&mut legacy, black_box(TOOL_CALL), &mut legacy_output);
        let (typed_time, legacy_time) = if round % 2 == 0 {
            (
                batch(ITERATIONS, &mut typed_task),
                batch(ITERATIONS, &mut legacy_task),
            )
        } else {
            let legacy_time = batch(ITERATIONS, &mut legacy_task);
            let typed_time = batch(ITERATIONS, &mut typed_task);
            (typed_time, legacy_time)
        };
        typed_samples.push(typed_time.as_secs_f64() * 1e9 / f64::from(ITERATIONS));
        legacy_samples.push(legacy_time.as_secs_f64() * 1e9 / f64::from(ITERATIONS));
    }

    let typed = median(&mut typed_samples);
    let legacy = median(&mut legacy_samples);
    println!(
        "{:<12} typed={typed:>9.2} ns value={legacy:>9.2} ns speedup={:>5.2}x",
        "typed/call",
        legacy / typed
    );
}

fn main() {
    compare("ping", PING);
    compare("initialize", INITIALIZE);
    compare("tools/list", TOOL_LIST);
    compare("tools/call", TOOL_CALL);
    compare_typed_tool();
}
