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

fn median_ratio(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
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
    let baseline = std::env::var_os("MCPORT_BASELINE_SERVER");
    let mut servers = vec![(
        "mcport-server".to_owned(),
        executable(directory, "mcport-server"),
    )];
    if let Some(baseline) = &baseline {
        servers.push(("mcport-baseline".to_owned(), PathBuf::from(baseline)));
    }
    if baseline.is_none() || std::env::var_os("MCPORT_BASELINE_ONLY").is_none() {
        servers.extend([
            (
                "rmcp-server".to_owned(),
                executable(directory, "rmcp-server"),
            ),
            (
                "rust-mcp-sdk-server".to_owned(),
                executable(directory, "rust-mcp-sdk-server"),
            ),
        ]);
    }
    let rounds = if baseline.is_some() { 9 } else { ROUNDS };
    let mut samples = vec![Vec::with_capacity(rounds); servers.len()];
    let mut paired_ratios = Vec::with_capacity(rounds);
    let mut response_bytes = vec![0; servers.len()];
    println!("\n{workload} ({ITERATIONS} requests after initialization)");

    for (index, (_, path)) in servers.iter().enumerate() {
        let (_, responses, bytes, last_response) = run_once(path, &input);
        assert_eq!(responses, ITERATIONS + 1);
        response_bytes[index] = bytes;
        validate_response(workload, &last_response);
    }
    for round in 0..rounds {
        let mut elapsed_by_server = vec![Duration::ZERO; servers.len()];
        for offset in 0..servers.len() {
            let index = (round + offset) % servers.len();
            let (elapsed, responses, bytes, _) = run_once(&servers[index].1, &input);
            assert_eq!(responses, ITERATIONS + 1);
            assert_eq!(bytes, response_bytes[index]);
            samples[index].push(elapsed);
            elapsed_by_server[index] = elapsed;
        }
        if baseline.is_some() {
            paired_ratios
                .push(elapsed_by_server[0].as_secs_f64() / elapsed_by_server[1].as_secs_f64());
        }
    }

    let medians: Vec<_> = samples.iter_mut().map(|sample| median(sample)).collect();
    let baseline = medians[0].as_secs_f64();
    let minimum_competitor_ratio = std::env::var("MCPORT_MIN_COMPETITOR_RATIO")
        .ok()
        .map(|value| {
            value
                .parse::<f64>()
                .expect("MCPORT_MIN_COMPETITOR_RATIO must be a number")
        });
    for (index, (server, _)) in servers.iter().enumerate() {
        let elapsed = medians[index];
        let per_request = elapsed.as_secs_f64() * 1e9 / ITERATIONS as f64;
        let throughput = ITERATIONS as f64 / elapsed.as_secs_f64();
        let relative = elapsed.as_secs_f64() / baseline;
        let bytes_per_response = response_bytes[index] as f64 / ITERATIONS as f64;
        println!(
            "{server:<24} {per_request:>10.2} ns/request {throughput:>10.0} req/s \
             {bytes_per_response:>7.1} B/response {relative:>6.2}x"
        );
        let enforced_ratio = (index > 0 && server != "mcport-baseline")
            .then_some(minimum_competitor_ratio)
            .flatten();
        if let Some(minimum) = enforced_ratio {
            assert!(
                relative >= minimum,
                "{server} was only {relative:.2}x slower than mcport \
                 (required at least {minimum:.2}x)"
            );
        }
    }
    if servers
        .get(1)
        .is_some_and(|(name, _)| name == "mcport-baseline")
    {
        let ratio = median_ratio(&mut paired_ratios);
        let delta_percent = (ratio - 1.0) * 100.0;
        println!("paired current vs baseline: {delta_percent:+.2}% latency");
        if let Ok(max_regression) = std::env::var("MCPORT_MAX_REGRESSION_PERCENT") {
            let max_regression = max_regression
                .parse::<f64>()
                .expect("MCPORT_MAX_REGRESSION_PERCENT must be a number");
            assert!(
                delta_percent <= max_regression,
                "{workload} regressed by {delta_percent:.2}% (limit {max_regression:.2}%)"
            );
        }
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
