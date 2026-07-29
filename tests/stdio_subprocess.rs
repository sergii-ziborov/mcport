#![cfg(feature = "subprocess-tests")]

use mcport::Value;
use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::Duration;

const SERVER: &str = env!("CARGO_BIN_EXE_mcport-test-server");

fn spawn(controlled: bool) -> Child {
    let mut command = Command::new(SERVER);
    if controlled {
        command.arg("--controlled");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcport test server")
}

fn exchange(controlled: bool, input: &[u8]) -> Output {
    let mut child = spawn(controlled);
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("write fixture input");
    child.wait_with_output().expect("wait for fixture")
}

fn json_lines(output: &[u8]) -> Vec<Value> {
    String::from_utf8(output.to_vec())
        .expect("stdout is UTF-8")
        .lines()
        .map(|line| blazingly_json::from_str(line).expect("stdout contains only JSON"))
        .collect()
}

#[test]
fn fragmented_legacy_lifecycle_modern_discovery_and_repeated_sessions_conform() {
    for id in 1..=3 {
        let mut child = spawn(false);
        let mut stdin = child.stdin.take().expect("piped stdin");
        write!(stdin, "{{\"jsonrpc\":\"2.0\",\"id\":{id},").expect("write first fragment");
        stdin.flush().expect("flush first fragment");
        thread::sleep(Duration::from_millis(10));
        stdin
            .write_all(
                concat!(
                    "\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",",
                    "\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
                    "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
                    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/list\"}\n"
                )
                .as_bytes(),
            )
            .expect("write second fragment");
        drop(stdin);
        let output = child.wait_with_output().expect("wait for fixture");
        assert!(output.status.success(), "{output:?}");
        let messages = json_lines(&output.stdout);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["id"], id);
        assert_eq!(messages[0]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(messages[1]["id"], 10);
        assert_eq!(messages[1]["result"]["tools"][0]["name"], "echo");
    }

    let modern = exchange(
        false,
        concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":\"discover\",\"method\":\"server/discover\",",
            "\"params\":{\"_meta\":{",
            "\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",",
            "\"io.modelcontextprotocol/clientInfo\":{\"name\":\"test\",\"version\":\"1\"},",
            "\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n"
        )
        .as_bytes(),
    );
    assert!(modern.status.success(), "{modern:?}");
    let messages = json_lines(&modern.stdout);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["result"]["resultType"], "complete");
    assert_eq!(messages[0]["result"]["supportedVersions"][0], "2026-07-28");
}

#[test]
fn partial_eof_huge_frames_and_invalid_utf8_are_bounded_errors() {
    let partial = exchange(false, br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    assert!(partial.status.success(), "{partial:?}");
    let messages = json_lines(&partial.stdout);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], Value::Null);
    assert_eq!(messages[0]["error"]["code"], -32_700);

    let mut oversized = vec![b'x'; 2048];
    oversized.push(b'\n');
    oversized.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");
    let oversized = exchange(false, &oversized);
    assert!(oversized.status.success(), "{oversized:?}");
    let messages = json_lines(&oversized.stdout);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["error"]["code"], -32_000);
    assert_eq!(messages[1]["id"], 2);

    let invalid = exchange(false, b"{\"bad\":\xff}\n");
    assert!(invalid.status.success(), "{invalid:?}");
    let messages = json_lines(&invalid.stdout);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["error"]["code"], -32_700);
}

#[test]
fn controlled_process_handles_cancellation_panic_progress_and_response_budgets() {
    let cancellation = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":\"cancel\",\"method\":\"tools/call\",",
        "\"params\":{\"name\":\"wait\",\"arguments\":{}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",",
        "\"params\":{\"requestId\":\"cancel\"}}\n"
    );
    let output = exchange(true, cancellation.as_bytes());
    assert!(output.status.success(), "{output:?}");
    assert!(!json_lines(&output.stdout)
        .iter()
        .any(|message| message["id"] == "cancel"));

    let panic = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",",
        "\"params\":{\"name\":\"panic\",\"arguments\":{}}}\n"
    );
    let output = exchange(true, panic.as_bytes());
    assert!(output.status.success(), "{output:?}");
    assert!(json_lines(&output.stdout)
        .iter()
        .any(|message| message["id"] == 2 && message["error"]["message"] == "handler panicked"));

    let progress = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",",
        "\"params\":{\"name\":\"progress\",\"arguments\":{},",
        "\"_meta\":{\"progressToken\":\"p\"}}}\n"
    );
    let output = exchange(true, progress.as_bytes());
    assert!(output.status.success(), "{output:?}");
    let messages = json_lines(&output.stdout);
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["method"] == "notifications/progress")
            .count(),
        2
    );
    assert!(messages
        .iter()
        .any(|message| message["id"] == 3 && message["result"]["isError"] == false));

    let oversized = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",",
        "\"params\":{\"name\":\"large\",\"arguments\":{}}}\n"
    );
    let output = exchange(true, oversized.as_bytes());
    assert!(output.status.success(), "{output:?}");
    let messages = json_lines(&output.stdout);
    assert!(messages
        .iter()
        .any(|message| message["id"] == 4 && message["error"]["code"] == -32_000));
}

#[test]
fn slow_stdout_reader_applies_backpressure_without_losing_responses() {
    const REQUESTS: usize = 5000;
    let mut child = spawn(false);
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = thread::spawn(move || {
        for id in 0..REQUESTS {
            writeln!(
                stdin,
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}"
            )
            .expect("write request");
        }
    });
    thread::sleep(Duration::from_millis(50));
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).expect("read stdout");
        bytes
    });
    writer.join().expect("request writer");
    let status = child.wait().expect("wait for slow-writer fixture");
    let output = reader.join().expect("response reader");
    assert!(status.success());
    assert_eq!(json_lines(&output).len(), REQUESTS);
}
