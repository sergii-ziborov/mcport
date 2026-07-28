use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const ITERATIONS: usize = 10_000;
const ROUNDS: usize = 5;
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"bench","version":"1.0"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
const TOOLS_LIST_TAIL: &str = r#","method":"tools/list"}"#;
const TOOLS_CALL_TAIL: &str = r#","method":"tools/call","params":{"name":"query_graph","arguments":{"query":"entry points","limit":20,"include_source":true}}}"#;

fn executable(directory: &Path, name: &str) -> PathBuf {
    let suffix = std::env::consts::EXE_SUFFIX;
    directory.join(format!("{name}{suffix}"))
}

fn input_for(message_tail: &str) -> Vec<u8> {
    let mut input =
        Vec::with_capacity(INITIALIZE.len() + INITIALIZED.len() + message_tail.len() * ITERATIONS);
    for line in [INITIALIZE, INITIALIZED] {
        input.extend_from_slice(line.as_bytes());
        input.push(b'\n');
    }
    for id in 1..=ITERATIONS {
        write!(&mut input, r#"{{"jsonrpc":"2.0","id":{id}"#).expect("write request prefix");
        input.extend_from_slice(message_tail.as_bytes());
        input.push(b'\n');
    }
    input
}

fn run_once(executable: &Path, input: &[u8]) -> (Duration, usize, usize, Vec<u8>) {
    let expected_responses = ITERATIONS + 1;
    let started = Instant::now();
    let mut child = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {}: {error}", executable.display()));
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut responses = 0;
        let mut response_bytes = 0;
        let mut last_response = Vec::new();
        let mut protocol_error = None;
        for line in BufReader::new(stdout).split(b'\n') {
            let line = line.expect("read benchmark response");
            if line.is_empty() {
                continue;
            }
            responses += 1;
            response_bytes += line.len() + 1;
            last_response = line;
            if last_response
                .windows(7)
                .any(|window| window == b"\"error\"")
            {
                protocol_error = Some(String::from_utf8_lossy(&last_response).into_owned());
                break;
            }
            if responses == expected_responses {
                break;
            }
        }
        result_tx
            .send((
                started.elapsed(),
                responses,
                response_bytes,
                last_response,
                protocol_error,
            ))
            .expect("send benchmark result");
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        BufReader::new(stderr)
            .read_to_end(&mut bytes)
            .expect("read benchmark stderr");
        bytes
    });
    stdin.write_all(input).expect("write benchmark requests");
    stdin.flush().expect("flush benchmark requests");
    let result = result_rx.recv_timeout(Duration::from_secs(30));
    drop(stdin);
    if result.is_err() {
        child.kill().expect("kill stalled benchmark server");
    }
    let status = child.wait().expect("wait for benchmark server");
    let stderr = stderr_reader.join().expect("stderr reader thread");
    reader.join().expect("response reader thread");
    let (elapsed, responses, response_bytes, last_response, protocol_error) =
        result.unwrap_or_else(|_| panic!("{} timed out", executable.display()));
    assert!(
        status.success(),
        "{} failed: {}",
        executable.display(),
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        protocol_error.is_none(),
        "{} returned an error: {}",
        executable.display(),
        protocol_error.unwrap_or_default()
    );
    (elapsed, responses, response_bytes, last_response)
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn validate_response(workload: &str, response: &[u8]) {
    let response: serde_json::Value =
        serde_json::from_slice(response).expect("last response is valid JSON");
    let id = response["id"]
        .as_u64()
        .expect("workload response has a numeric id");
    assert!((1..=ITERATIONS as u64).contains(&id));
    match workload {
        "tools/list" => assert!(
            response["result"]["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "query_graph")),
            "tools/list must expose query_graph"
        ),
        "tools/call" => {
            let result = &response["result"]["structuredContent"];
            assert_eq!(result["nodes"], 12);
            assert_eq!(result["query"], "entry points");
            assert_eq!(result["limit"], 20);
            assert_eq!(result["include_source"], true);
        }
        _ => unreachable!("known workload"),
    }
}

fn compare(directory: &Path, workload: &str, message_tail: &str) {
    let input = input_for(message_tail);
    let servers = ["mcport-server", "rmcp-server", "rust-mcp-sdk-server"];
    let mut samples: [Vec<Duration>; 3] = std::array::from_fn(|_| Vec::with_capacity(ROUNDS));
    let mut response_bytes = [0; 3];
    println!("\n{workload} ({ITERATIONS} requests after initialization)");

    for (index, server) in servers.iter().enumerate() {
        let path = executable(directory, server);
        let (_, responses, bytes, last_response) = run_once(&path, &input);
        assert_eq!(responses, ITERATIONS + 1);
        response_bytes[index] = bytes;
        validate_response(workload, &last_response);
    }
    for round in 0..ROUNDS {
        for offset in 0..servers.len() {
            let index = (round + offset) % servers.len();
            let path = executable(directory, servers[index]);
            let (elapsed, responses, bytes, _) = run_once(&path, &input);
            assert_eq!(responses, ITERATIONS + 1);
            assert_eq!(bytes, response_bytes[index]);
            samples[index].push(elapsed);
        }
    }

    let medians = samples.each_mut().map(|sample| median(sample));
    let baseline = medians[0].as_secs_f64();
    for (index, server) in servers.iter().enumerate() {
        let elapsed = medians[index];
        let per_request = elapsed.as_secs_f64() * 1e9 / ITERATIONS as f64;
        let throughput = ITERATIONS as f64 / elapsed.as_secs_f64();
        let relative = elapsed.as_secs_f64() / baseline;
        let bytes_per_response = response_bytes[index] as f64 / ITERATIONS as f64;
        println!(
            "{server:<24} {per_request:>10.2} ns/request {throughput:>10.0} req/s \
             {bytes_per_response:>7.1} B/response {relative:>6.2}x"
        );
    }
}

fn main() {
    let directory = std::env::current_exe()
        .expect("current executable")
        .parent()
        .expect("release directory")
        .to_path_buf();
    compare(&directory, "tools/list", TOOLS_LIST_TAIL);
    compare(&directory, "tools/call", TOOLS_CALL_TAIL);
}
