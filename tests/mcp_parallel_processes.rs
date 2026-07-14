#![cfg(feature = "mcp")]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

struct McpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<String>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<Vec<u8>>>,
}

struct ProcessReport {
    status: ExitStatus,
    timed_out: bool,
    stderr: String,
}

impl McpProcess {
    fn spawn(project_root: &std::path::Path, base_name: &str) -> Self {
        let base = format!(".memoryx/bases/{base_name}");
        let mut child = Command::new(env!("CARGO_BIN_EXE_memoryx"))
            .current_dir(project_root)
            .args(["serve", "--base", &base, "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("failed to spawn memoryx MCP process: {error}"));

        let stdout = child.stdout.take().expect("MCP child stdout must be piped");
        let (response_tx, response_rx) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if response_tx.send(line).is_err() {
                    break;
                }
            }
        });

        let stderr = child.stderr.take().expect("MCP child stderr must be piped");
        let stderr_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = BufReader::new(stderr).read_to_end(&mut bytes);
            bytes
        });

        Self {
            stdin: Some(child.stdin.take().expect("MCP child stdin must be piped")),
            child,
            responses: response_rx,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
        }
    }

    fn request(&mut self, request: Value) -> Result<Value, String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "MCP stdin is closed".to_string())?;
        writeln!(stdin, "{request}").map_err(|error| format!("write failed: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("flush failed: {error}"))?;

        let line = self
            .responses
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => {
                    format!("timed out after {REQUEST_TIMEOUT:?} waiting for MCP response")
                }
                RecvTimeoutError::Disconnected => {
                    "MCP stdout closed before a response was received".to_string()
                }
            })?;
        serde_json::from_str(&line).map_err(|error| format!("invalid MCP JSON response: {error}"))
    }

    fn collect_report(&mut self, status: ExitStatus, timed_out: bool) -> ProcessReport {
        let _ = self
            .stdout_reader
            .take()
            .expect("MCP stdout reader must exist")
            .join();
        let stderr = self
            .stderr_reader
            .take()
            .expect("MCP stderr reader must exist")
            .join()
            .expect("MCP stderr reader must not panic");

        ProcessReport {
            status,
            timed_out,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }

    fn finish(&mut self) -> ProcessReport {
        self.stdin.take();
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let mut timed_out = false;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    timed_out = true;
                    let _ = self.child.kill();
                    break self
                        .child
                        .wait()
                        .expect("killed MCP child must be waitable");
                }
                Err(error) => panic!("failed to poll MCP child: {error}"),
            }
        };

        self.collect_report(status, timed_out)
    }

    fn force_kill(&mut self) -> ProcessReport {
        self.stdin.take();
        self.child
            .kill()
            .expect("initialized MCP child must be killable");
        let status = self
            .child
            .wait()
            .expect("force-killed MCP child must be waitable");
        self.collect_report(status, false)
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "mcp-parallel-processes-test", "version": "0.1"}
        }
    })
}

fn target_base_ref(base_index: usize) -> String {
    format!("target-{base_index}")
}

fn target_base_path(base_index: usize) -> String {
    format!(".memoryx/bases/base-{base_index}")
}

fn writer_lease_path(project_root: &std::path::Path, base_index: usize) -> std::path::PathBuf {
    project_root
        .join(target_base_path(base_index))
        .join(".memoryx.writer.lock")
}

fn target_base_marker(root_index: usize, base_index: usize) -> String {
    format!("routingmarkerroot{root_index}base{base_index}")
}

fn atom_count(root_index: usize, base_index: usize) -> usize {
    (root_index + 1) * 10 + base_index + 1
}

fn connect_base_request(id: u64, base_ref: &str, path: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "connect_base",
            "arguments": {"base_ref": base_ref, "path": path}
        }
    })
}

fn list_bases_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": "list_bases", "arguments": {}}
    })
}

fn batch_ingest_request(id: u64, base_ref: &str, marker: &str, count: usize) -> Value {
    let atoms = (0..count)
        .map(|index| {
            json!({
                "atom_type": "FACT",
                "symbols": if index == 0 {
                    json!(["filler", marker, format!("atommarker{index}")])
                } else {
                    json!(["filler", format!("atommarker{index}"), "batchatom"])
                },
                "claims": [{
                    "subj": 1,
                    "pred": 2,
                    "obj_tag": 0,
                    "obj_val": index as u64 + 1,
                    "qualifiers_mask": 0
                }]
            })
        })
        .collect::<Vec<_>>();

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "batch_ingest",
            "arguments": {"base_ref": base_ref, "atoms": atoms}
        }
    })
}

fn search_request(id: u64, base_ref: &str, term: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "search_lex",
            "arguments": {"base_ref": base_ref, "term": term}
        }
    })
}

fn history_request(id: u64, base_ref: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "history",
            "arguments": {"base_ref": base_ref, "limit": 10}
        }
    })
}

fn persisted_atom_count(history: &str) -> usize {
    history
        .lines()
        .find_map(|line| line.strip_prefix("Atom IDs: "))
        .map(|ids| ids.split(", ").filter(|id| !id.is_empty()).count())
        .unwrap_or(0)
}

fn lexical_atom_count(search_result: &str) -> usize {
    search_result
        .lines()
        .find_map(|line| line.strip_prefix("count="))
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("MCP lexical response has no count: {search_result}"))
}

fn response_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("MCP response has no text content: {response}"))
}

fn assert_success(response: &Value) {
    assert!(
        response.get("error").is_none(),
        "MCP request failed: {response}"
    );
}

fn assert_connected_bases(response: &Value, expected_refs: &[String]) {
    assert_success(response);
    let listing: Value = serde_json::from_str(response_text(response))
        .unwrap_or_else(|error| panic!("invalid list_bases payload: {error}"));
    let bases = listing["bases"]
        .as_array()
        .unwrap_or_else(|| panic!("list_bases payload has no bases array: {listing}"));

    for expected_ref in expected_refs {
        let base = bases
            .iter()
            .find(|base| base["base_ref"].as_str() == Some(expected_ref.as_str()))
            .unwrap_or_else(|| panic!("base {expected_ref} missing from list_bases: {listing}"));
        assert_eq!(
            base["connected"].as_bool(),
            Some(true),
            "base {expected_ref} is not connected: {listing}"
        );
    }
}

fn assert_routed_search(
    process: &mut McpProcess,
    request_id: u64,
    base_ref: &str,
    marker: &str,
    expected_count: usize,
) {
    let response = process
        .request(search_request(request_id, base_ref, marker))
        .unwrap_or_else(|error| panic!("lexical search failed for {base_ref}: {error}"));
    assert_success(&response);
    let text = response_text(&response);
    assert_eq!(
        lexical_atom_count(text),
        expected_count,
        "explicit base_ref {base_ref} returned the wrong lexical marker count: {text}"
    );
}

fn assert_routed_history(
    process: &mut McpProcess,
    request_id: u64,
    base_ref: &str,
    expected_count: usize,
) {
    let response = process
        .request(history_request(request_id, base_ref))
        .unwrap_or_else(|error| panic!("history failed for {base_ref}: {error}"));
    assert_success(&response);
    let text = response_text(&response);
    assert_eq!(
        persisted_atom_count(text),
        expected_count,
        "explicit base_ref {base_ref} returned the wrong persisted atom count: {text}"
    );
}

#[test]
fn parallel_mcp_processes_are_isolated_persistent_and_exclusive() {
    let roots = [
        (TempDir::new().expect("temp root alpha"), 1usize),
        (TempDir::new().expect("temp root beta"), 2usize),
        (TempDir::new().expect("temp root gamma"), 4usize),
    ];

    let mut processes = Vec::new();
    for (root, base_count) in &roots {
        let mut process = McpProcess::spawn(root.path(), "anchor");
        let initialized = process
            .request(initialize_request(1))
            .unwrap_or_else(|error| panic!("failed to initialize MCP client: {error}"));
        assert_success(&initialized);

        let mut expected_refs = Vec::new();
        for base_index in 0..*base_count {
            let base_ref = target_base_ref(base_index);
            let response = process
                .request(connect_base_request(
                    2 + base_index as u64,
                    &base_ref,
                    &target_base_path(base_index),
                ))
                .unwrap_or_else(|error| panic!("failed to connect {base_ref}: {error}"));
            assert_success(&response);
            let connected: Value = serde_json::from_str(response_text(&response))
                .unwrap_or_else(|error| panic!("invalid connect_base payload: {error}"));
            assert_eq!(
                connected["base_ref"].as_str(),
                Some(base_ref.as_str()),
                "connect_base returned the wrong base reference: {connected}"
            );
            expected_refs.push(base_ref);
        }

        let listed = process
            .request(list_bases_request(20 + *base_count as u64))
            .expect("list_bases request failed");
        assert_connected_bases(&listed, &expected_refs);
        processes.push(process);
    }

    let mut duplicate = McpProcess::spawn(roots[0].0.path(), "base-0");
    let duplicate_report = duplicate.finish();
    assert!(
        !duplicate_report.timed_out,
        "same-root process was not rejected within {EXIT_TIMEOUT:?}; it may have acquired no lease"
    );
    assert!(
        !duplicate_report.status.success(),
        "same-root process unexpectedly started successfully; stderr: {}",
        duplicate_report.stderr
    );
    let duplicate_diagnostics = duplicate_report.stderr.to_ascii_lowercase();
    assert!(
        ["lease", "lock", "exclusive", "already in use", "busy"]
            .iter()
            .any(|marker| duplicate_diagnostics.contains(marker)),
        "same-root rejection did not identify an exclusive lease/lock: {}",
        duplicate_report.stderr
    );

    for (root_index, ((_, base_count), process)) in
        roots.iter().zip(processes.iter_mut()).enumerate()
    {
        for base_index in 0..*base_count {
            let base_ref = target_base_ref(base_index);
            let marker = target_base_marker(root_index, base_index);
            let count = atom_count(root_index, base_index);
            let response = process
                .request(batch_ingest_request(
                    100 + base_index as u64,
                    &base_ref,
                    &marker,
                    count,
                ))
                .unwrap_or_else(|error| panic!("batch ingest failed for {base_ref}: {error}"));
            assert_success(&response);
            let text = response_text(&response);
            assert!(
                text.contains(&format!("Total: {count}\nSuccess: {count}")),
                "unexpected batch result for {base_ref}: {text}"
            );

            assert_routed_history(process, 300 + base_index as u64, &base_ref, count);
        }
    }

    for (client_root_index, ((_, client_base_count), process)) in
        roots.iter().zip(processes.iter_mut()).enumerate()
    {
        for (owner_root_index, (_, owner_base_count)) in roots.iter().enumerate() {
            for owner_base_index in 0..*owner_base_count {
                let marker = target_base_marker(owner_root_index, owner_base_index);
                for query_base_index in 0..*client_base_count {
                    let query_base_ref = target_base_ref(query_base_index);
                    let expected = usize::from(
                        client_root_index == owner_root_index
                            && query_base_index == owner_base_index,
                    );
                    assert_routed_search(
                        process,
                        600 + (client_root_index * 1000
                            + owner_root_index * 100
                            + owner_base_index * 10
                            + query_base_index) as u64,
                        &query_base_ref,
                        &marker,
                        expected,
                    );
                }
            }
        }
    }

    for process in &mut processes {
        let report = process.finish();
        assert!(
            !report.timed_out && report.status.success(),
            "MCP process did not exit cleanly: timed_out={}, status={}, stderr={}",
            report.timed_out,
            report.status,
            report.stderr
        );
    }

    let lease_path = writer_lease_path(roots[0].0.path(), 0);
    let mut crashed_owner = McpProcess::spawn(roots[0].0.path(), "base-0");
    let initialized = crashed_owner
        .request(initialize_request(700))
        .expect("crash owner failed to initialize");
    assert_success(&initialized);
    assert!(
        lease_path.exists(),
        "initialized owner did not create the persistent writer lease file: {}",
        lease_path.display()
    );

    let crash_report = crashed_owner.force_kill();
    assert!(
        !crash_report.status.success(),
        "force-killed owner unexpectedly exited successfully"
    );
    assert!(
        lease_path.exists(),
        "force-killed owner removed the persistent writer lease file: {}",
        lease_path.display()
    );

    let mut after_crash = McpProcess::spawn(roots[0].0.path(), "base-0");
    let reopened = after_crash
        .request(initialize_request(701))
        .expect("fresh process could not reopen the crashed owner's root");
    assert_success(&reopened);
    assert!(
        lease_path.exists(),
        "fresh owner reopened without the persistent writer lease file"
    );
    let reopen_report = after_crash.finish();
    assert!(
        !reopen_report.timed_out && reopen_report.status.success(),
        "post-crash owner did not exit cleanly: timed_out={}, status={}, stderr={}",
        reopen_report.timed_out,
        reopen_report.status,
        reopen_report.stderr
    );

    for (root_index, (root, base_count)) in roots.iter().enumerate() {
        let mut process = McpProcess::spawn(root.path(), "anchor");
        let initialized = process
            .request(initialize_request(400 + root_index as u64))
            .unwrap_or_else(|error| panic!("failed to reopen root {root_index}: {error}"));
        assert_success(&initialized);

        let mut expected_refs = Vec::new();
        for base_index in 0..*base_count {
            let base_ref = target_base_ref(base_index);
            let response = process
                .request(connect_base_request(
                    410 + base_index as u64,
                    &base_ref,
                    &target_base_path(base_index),
                ))
                .unwrap_or_else(|error| panic!("failed to reconnect {base_ref}: {error}"));
            assert_success(&response);
            expected_refs.push(base_ref);
        }

        let listed = process
            .request(list_bases_request(450 + *base_count as u64))
            .expect("list_bases request failed after restart");
        assert_connected_bases(&listed, &expected_refs);

        for base_index in 0..*base_count {
            let base_ref = target_base_ref(base_index);
            let marker = target_base_marker(root_index, base_index);
            let count = atom_count(root_index, base_index);
            assert_routed_search(&mut process, 500 + base_index as u64, &base_ref, &marker, 1);
            assert_routed_history(&mut process, 550 + base_index as u64, &base_ref, count);
        }

        let report = process.finish();
        assert!(
            !report.timed_out && report.status.success(),
            "reopened MCP process did not exit cleanly for root {root_index}: timed_out={}, status={}, stderr={}",
            report.timed_out,
            report.status,
            report.stderr
        );
    }
}
