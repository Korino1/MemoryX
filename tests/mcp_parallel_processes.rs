#![cfg(feature = "mcp")]

use std::collections::HashSet;
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

    fn send(&mut self, message: Value) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "MCP stdin is closed".to_string())?;
        writeln!(stdin, "{message}").map_err(|error| format!("write failed: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("flush failed: {error}"))
    }

    fn request(&mut self, request: Value) -> Result<Value, String> {
        self.send(request)?;

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

    fn expect_no_response(&self, timeout: Duration) -> Result<(), String> {
        match self.responses.recv_timeout(timeout) {
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                Err("MCP stdout closed while waiting for notification silence".to_string())
            }
            Ok(line) => Err(format!(
                "notification unexpectedly produced a protocol message: {line}"
            )),
        }
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

fn codex_initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {
                "elicitation": {
                    "form": {},
                    "url": {}
                }
            },
            "clientInfo": {
                "name": "codex-mcp-client",
                "title": "Codex",
                "version": "0.144.4"
            }
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

fn tool_request(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    })
}

fn ingested_atom_id(response: &Value) -> String {
    response_text(response)
        .lines()
        .find_map(|line| line.strip_prefix("Atom ID: "))
        .unwrap_or_else(|| panic!("ingest response has no atom id: {response}"))
        .to_string()
}

#[test]
fn codex_lifecycle_ignores_initialized_notification_before_tools_list() {
    let root = TempDir::new().expect("Codex lifecycle temp root");
    let mut process = McpProcess::spawn(root.path(), "codex-lifecycle");

    let initialized = process
        .request(codex_initialize_request(1001))
        .expect("Codex initialize request failed");
    assert_eq!(
        initialized,
        json!({
            "jsonrpc": "2.0",
            "id": 1001,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "memoryx",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
        "initialize response must match the current Codex MCP shape exactly"
    );

    process
        .send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))
        .expect("failed to send notifications/initialized");
    process
        .expect_no_response(Duration::from_millis(300))
        .expect("notifications/initialized must not produce a response or blank line");

    let tools = process
        .request(json!({
            "jsonrpc": "2.0",
            "id": "codex-tools-list",
            "method": "tools/list",
            "params": {}
        }))
        .expect("Codex tools/list request failed after initialized notification");
    assert_success(&tools);
    assert_eq!(tools["id"], json!("codex-tools-list"));
    assert_eq!(tools["result"]["tools"].as_array().map(Vec::len), Some(42));
    assert!(
        tools["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "active_base"))
    );

    let active_base = process
        .request(json!({
            "jsonrpc": "2.0",
            "id": "codex-active-base",
            "method": "tools/call",
            "params": {
                "name": "active_base",
                "arguments": {}
            }
        }))
        .expect("Codex active_base call failed after tools/list");
    assert_success(&active_base);
    assert_eq!(active_base["id"], json!("codex-active-base"));
    assert!(response_text(&active_base).contains("\"active_base_ref\": \"active\""));

    let report = process.finish();
    assert!(
        !report.timed_out && report.status.success(),
        "Codex lifecycle MCP process did not exit cleanly: timed_out={}, status={}, stderr={}",
        report.timed_out,
        report.status,
        report.stderr
    );
}

#[test]
fn source_projection_and_context_lineage_survive_mcp_process_reopen() {
    let root = TempDir::new().expect("MCP reopen temp root");
    let base_name = "mx01-reopen";
    let atom_id;

    {
        let mut process = McpProcess::spawn(root.path(), base_name);
        process
            .request(codex_initialize_request(2000))
            .expect("initialize first MCP process");

        let ingest = process
            .request(tool_request(
                2001,
                "ingest",
                json!({
                    "atom_type": "FACT",
                    "symbols": ["mx01sourceprojection"],
                    "claims": [{
                        "subj": 7,
                        "pred": 8,
                        "obj_tag": 3,
                        "obj_val": 9,
                        "qualifiers_mask": 0
                    }]
                }),
            ))
            .expect("ingest source-backed atom");
        atom_id = ingested_atom_id(&ingest);

        let source = process
            .request(tool_request(
                2002,
                "register_source",
                json!({
                    "kind": "file",
                    "label": "MX-01 source projection fixture",
                    "path": "docs/mx01-source.txt",
                    "line_start": 10,
                    "line_end": 20,
                    "source_version": "test"
                }),
            ))
            .expect("register source");
        assert!(response_text(&source).contains("Source ID: 1"));

        process
            .request(tool_request(
                2003,
                "attach_atom_source",
                json!({"atom_id": atom_id, "source_id": 1}),
            ))
            .expect("attach source to atom");

        let root_context = process
            .request(tool_request(
                2004,
                "create_context",
                json!({"policy_id": 0}),
            ))
            .expect("create root context");
        assert!(response_text(&root_context).contains("created_ctx=0"));
        let branch = process
            .request(tool_request(
                2005,
                "branch_context",
                json!({"parent_ctx": 0, "reason": "Hypothesis", "policy_id": 1}),
            ))
            .expect("create hypothesis branch");
        assert!(response_text(&branch).contains("Created branch context: 1"));

        let provenance = process
            .request(tool_request(
                2006,
                "get_provenance_path",
                json!({"atom_id": atom_id}),
            ))
            .expect("get in-process provenance");
        let provenance: Value =
            serde_json::from_str(response_text(&provenance)).expect("provenance JSON");
        assert_eq!(provenance["direct_evidence"].as_array().unwrap().len(), 1);
        assert_eq!(provenance["direct_evidence"][0]["source_id"], 1);
        assert_eq!(
            provenance["direct_evidence"][0]["source_location"]["path"],
            "docs/mx01-source.txt"
        );
        assert_eq!(
            provenance["nodes"][0]["evidence_links"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let report = process.finish();
        assert!(!report.timed_out, "first MCP process timed out");
        assert!(
            report.status.success(),
            "first MCP stderr: {}",
            report.stderr
        );
    }

    let mut reopened = McpProcess::spawn(root.path(), base_name);
    reopened
        .request(codex_initialize_request(2010))
        .expect("initialize reopened MCP process");

    let sources = reopened
        .request(tool_request(2011, "list_sources", json!({})))
        .expect("list persisted sources");
    assert!(response_text(&sources).contains("MX-01 source projection fixture"));

    let provenance = reopened
        .request(tool_request(
            2012,
            "get_provenance_path",
            json!({"atom_id": atom_id}),
        ))
        .expect("get reopened provenance");
    let provenance: Value =
        serde_json::from_str(response_text(&provenance)).expect("reopened provenance JSON");
    assert_eq!(provenance["direct_evidence"].as_array().unwrap().len(), 1);
    assert_eq!(provenance["direct_evidence"][0]["source_id"], 1);
    assert_eq!(
        provenance["direct_evidence"][0]["source_location"]["line_range"],
        json!([10, 20])
    );
    assert_eq!(
        provenance["nodes"][0]["evidence_links"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let answer = reopened
        .request(tool_request(
            2013,
            "query",
            json!({"query_text": "mx01sourceprojection", "ctx_id": 0}),
        ))
        .expect("query reopened source-backed atom");
    let answer: Value = serde_json::from_str(response_text(&answer)).expect("AnswerPack JSON");
    assert_single_source_graph(&answer["graph"], &atom_id);
    assert_eq!(answer["coverage_report"]["evidence_ref_count"], 1);
    assert_eq!(answer["coverage_report"]["evidence_record_count"], 1);
    assert_eq!(answer["coverage_report"]["source_link_count"], 1);
    assert_eq!(
        answer["coverage_report"]["evidence_ref_count"],
        answer["graph"]["evidence_ref_count"]
    );
    assert_eq!(
        answer["coverage_report"]["evidence_record_count"],
        answer["graph"]["evidence_record_count"]
    );
    assert_eq!(
        answer["coverage_report"]["source_link_count"],
        answer["graph"]["source_link_count"]
    );
    assert_eq!(answer["evidence_records"].as_array().unwrap().len(), 1);
    assert_eq!(answer["evidence_records"][0]["source_id"], 1);
    let source_node = answer["graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["atom_id"] == atom_id)
        .expect("source-backed atom must be present in serialized AnswerGraph");
    assert_eq!(source_node["source_link_count"], 1);
    assert_eq!(source_node["direct_evidence"][0]["source_id"], 1);
    assert_eq!(
        source_node["direct_evidence"][0]["source_location"]["path"],
        "docs/mx01-source.txt"
    );
    assert!(
        answer["claims"]
            .as_array()
            .unwrap()
            .iter()
            .any(|claim| !claim["provenance_path"].as_array().unwrap().is_empty())
    );

    let explanation = reopened
        .request(tool_request(
            2014,
            "explain_answer_graph",
            json!({"query_text": "mx01sourceprojection", "ctx_id": 0}),
        ))
        .expect("explain reopened source-backed atom");
    let explanation: Value =
        serde_json::from_str(response_text(&explanation)).expect("AnswerGraph explanation JSON");
    assert_single_source_graph(&explanation["graph"], &atom_id);
    assert_eq!(explanation["coverage_report"]["evidence_ref_count"], 1);
    assert_eq!(explanation["coverage_report"]["evidence_record_count"], 1);
    assert_eq!(explanation["coverage_report"]["source_link_count"], 1);
    assert_eq!(
        explanation["coverage_report"]["evidence_ref_count"],
        explanation["graph"]["evidence_ref_count"]
    );
    assert_eq!(
        explanation["coverage_report"]["evidence_record_count"],
        explanation["graph"]["evidence_record_count"]
    );
    assert_eq!(
        explanation["coverage_report"]["source_link_count"],
        explanation["graph"]["source_link_count"]
    );
    assert_eq!(answer["graph"], explanation["graph"]);
    assert_eq!(
        answer["graph"]["nodes"][0]["direct_evidence"][0]["observed_at_unix_ns"],
        answer["evidence_records"][0]["observed_at_unix_ns"]
    );
    assert!(
        answer["evidence_records"][0]["observed_at_unix_ns"]
            .as_u64()
            .is_some_and(|timestamp| timestamp > 0)
    );

    let repeated_answer = reopened
        .request(tool_request(
            2015,
            "query",
            json!({"query_text": "mx01sourceprojection", "ctx_id": 0}),
        ))
        .expect("repeat query for deterministic graph");
    let repeated_answer: Value =
        serde_json::from_str(response_text(&repeated_answer)).expect("repeated AnswerPack JSON");
    assert_eq!(answer["graph"], repeated_answer["graph"]);

    let repeated_explanation = reopened
        .request(tool_request(
            2016,
            "explain_answer_graph",
            json!({"query_text": "mx01sourceprojection", "ctx_id": 0}),
        ))
        .expect("repeat explanation for deterministic graph");
    let repeated_explanation: Value = serde_json::from_str(response_text(&repeated_explanation))
        .expect("repeated AnswerGraph explanation JSON");
    assert_eq!(answer["graph"], repeated_explanation["graph"]);

    let contexts = reopened
        .request(tool_request(2017, "list_contexts", json!({})))
        .expect("list reopened contexts");
    let contexts = response_text(&contexts);
    assert!(contexts.contains("Total: 2"), "{contexts}");
    assert!(
        contexts.contains("ID: 1, Status: active, Parent: 0"),
        "{contexts}"
    );
    assert!(contexts.contains("Branch reason: hypothesis"), "{contexts}");

    let report = reopened.finish();
    assert!(!report.timed_out, "reopened MCP process timed out");
    assert!(
        report.status.success(),
        "reopened MCP stderr: {}",
        report.stderr
    );
}

#[test]
fn predicate_bootstrap_and_symbol_only_multi_source_query_survive_reopen() {
    let root = TempDir::new().expect("symbol-only temp root");
    let base_name = "symbol-only-multi-source";
    let symbol = "val_08a_3r_gb1_next";
    let atom_id;
    let predicate_id;
    let managed_claim_atom;
    let managed_relation_atom;

    {
        let mut process = McpProcess::spawn(root.path(), base_name);
        process
            .request(codex_initialize_request(3000))
            .expect("initialize authoring MCP process");

        let predicate_contract = json!({
            "stable_key": "test:depends_on",
            "canonical_name": "depends_on",
            "description": "Subject requires object before completion.",
            "direction": "directed",
            "inverse_stable_key": "test:required_by",
            "cardinality": "many_to_many"
        });
        let registered = process
            .request(tool_request(
                3001,
                "register_predicate",
                predicate_contract.clone(),
            ))
            .expect("register managed predicate");
        let registered: Value =
            serde_json::from_str(response_text(&registered)).expect("predicate JSON");
        predicate_id = registered["predicate_id"]
            .as_u64()
            .expect("deterministic predicate id");
        assert!(predicate_id >= 0x8000_0000u64);
        let stable_identity = registered["stable_identity"].clone();

        let repeated = process
            .request(tool_request(3002, "register_predicate", predicate_contract))
            .expect("idempotent predicate registration");
        let repeated: Value =
            serde_json::from_str(response_text(&repeated)).expect("repeated predicate JSON");
        assert_eq!(repeated["predicate_id"], registered["predicate_id"]);
        assert_eq!(repeated["stable_identity"], stable_identity);

        let conflict = process
            .request(tool_request(
                3003,
                "register_predicate",
                json!({
                    "stable_key": "test:depends_on",
                    "canonical_name": "depends_on",
                    "description": "Conflicting semantics must be rejected."
                }),
            ))
            .expect("conflicting predicate response");
        assert_eq!(conflict["error"]["code"], -32602);

        for (id, name) in [(3030, "managed subject"), (3031, "managed object")] {
            assert_success(
                &process
                    .request(tool_request(
                        id,
                        "create_entity",
                        json!({"canonical_name": name, "entity_type": "test"}),
                    ))
                    .expect("create managed entity"),
            );
        }
        let managed_claim = process
            .request(tool_request(
                3032,
                "add_claim",
                json!({
                    "entity_id": 1,
                    "predicate": predicate_id,
                    "object": 77,
                    "object_tag": "SYM",
                    "ctx_id": 0
                }),
            ))
            .expect("managed add_claim");
        assert_success(&managed_claim);
        managed_claim_atom = ingested_atom_id(&managed_claim);
        let managed_relation = process
            .request(tool_request(
                3033,
                "assert_relation",
                json!({
                    "subject": 1,
                    "predicate": predicate_id,
                    "object": 2,
                    "ctx_id": 0
                }),
            ))
            .expect("managed assert_relation");
        assert_success(&managed_relation);
        managed_relation_atom = ingested_atom_id(&managed_relation);

        let ingest = process
            .request(tool_request(
                3004,
                "ingest",
                json!({
                    "atom_type": "DECISION",
                    "symbols": [symbol, "val_08a_3r_gb0_design_frozen"],
                    "claims": []
                }),
            ))
            .expect("ingest zero-claim decision");
        atom_id = ingested_atom_id(&ingest);

        for (id, label, path, start, end) in [
            (
                3005,
                "implementation plan",
                "HPF_IMPLEMENTATION_PLAN.md",
                389,
                397,
            ),
            (
                3006,
                "implementation tracker",
                "IMPLEMENTATION_TRACKING.md",
                333,
                414,
            ),
        ] {
            let source = process
                .request(tool_request(
                    id,
                    "register_source",
                    json!({
                        "kind": "file",
                        "label": label,
                        "path": path,
                        "line_start": start,
                        "line_end": end,
                        "source_version": "isolated-test"
                    }),
                ))
                .expect("register source");
            assert_success(&source);
        }

        for (id, source_id) in [(3007, 1), (3008, 2), (3009, 1)] {
            let attached = process
                .request(tool_request(
                    id,
                    "attach_atom_source",
                    json!({"atom_id": atom_id, "source_id": source_id}),
                ))
                .expect("attach accumulating source");
            assert_success(&attached);
        }

        let compiled = process
            .request(tool_request(
                3010,
                "compile_query_contract",
                json!({"query_text": format!("what follows {symbol} and why")}),
            ))
            .expect("compile natural symbol query");
        assert!(response_text(&compiled).contains(symbol));

        let report = process.finish();
        assert!(
            !report.timed_out && report.status.success(),
            "authoring MCP process failed: {}",
            report.stderr
        );
    }

    let mut reopened = McpProcess::spawn(root.path(), base_name);
    reopened
        .request(codex_initialize_request(3020))
        .expect("initialize reopened symbol-only MCP process");

    let predicates = reopened
        .request(tool_request(3021, "list_predicates", json!({})))
        .expect("list persisted predicates");
    let predicates: Value =
        serde_json::from_str(response_text(&predicates)).expect("predicate list JSON");
    assert_eq!(predicates.as_array().map(Vec::len), Some(1));
    let resolved = reopened
        .request(tool_request(
            3022,
            "resolve_predicate",
            json!({"name_or_key": "DEPENDS_ON"}),
        ))
        .expect("resolve predicate by canonical name");
    let resolved: Value =
        serde_json::from_str(response_text(&resolved)).expect("resolved predicate JSON");
    assert_eq!(resolved["predicate_id"], predicate_id);
    let inspected = reopened
        .request(tool_request(
            3023,
            "get_predicate",
            json!({"predicate_id": predicate_id}),
        ))
        .expect("inspect predicate by id");
    assert_eq!(
        serde_json::from_str::<Value>(response_text(&inspected)).unwrap(),
        resolved
    );

    for (id, managed_atom, expected_tag, expected_value) in [
        (3034, managed_claim_atom, "SYM", json!({"Sym": 77})),
        (3035, managed_relation_atom, "NODENUM", json!({"U64": 2})),
    ] {
        let response = reopened
            .request(tool_request(
                id,
                "query",
                json!({"contract": {"intent": "lookup", "targets": [{"label": format!("atom:{managed_atom}")}]}, "ctx_id": 0}),
            ))
            .expect("query managed authored atom after reopen");
        let answer: Value = serde_json::from_str(response_text(&response)).unwrap();
        assert!(answer["claims"].as_array().unwrap().iter().any(|claim| {
            claim["pred"].as_u64() == Some(predicate_id)
                && claim["obj_tag"] == expected_tag
                && claim["obj_value"] == expected_value
        }));
    }

    let provenance = reopened
        .request(tool_request(
            3024,
            "get_provenance_path",
            json!({"atom_id": atom_id}),
        ))
        .expect("get multi-source provenance after reopen");
    let provenance: Value =
        serde_json::from_str(response_text(&provenance)).expect("multi-source provenance JSON");
    let source_ids = provenance["direct_evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|evidence| evidence["source_id"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(source_ids, vec![1, 2]);
    assert_eq!(
        provenance["nodes"][0]["evidence_links"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );

    let lexical = reopened
        .request(search_request(3025, "active", symbol))
        .expect("lexical search for zero-claim symbol");
    assert_eq!(lexical_atom_count(response_text(&lexical)), 1);

    let query = reopened
        .request(tool_request(
            3026,
            "query",
            json!({"query_text": format!("what follows {symbol} and why"), "ctx_id": 0}),
        ))
        .expect("query zero-claim decision after reopen");
    let answer: Value = serde_json::from_str(response_text(&query)).expect("AnswerPack JSON");
    assert_eq!(answer["status"], "InsufficientEvidence");
    assert_eq!(answer["claims"].as_array().map(Vec::len), Some(0));
    assert_eq!(answer["graph"]["node_count"], 1);
    assert_eq!(answer["graph"]["evidence_record_count"], 2);
    assert_eq!(answer["graph"]["source_link_count"], 2);
    assert_eq!(answer["evidence_records"].as_array().map(Vec::len), Some(2));
    assert!(
        answer["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|limitation| {
                limitation["description"]
                    .as_str()
                    .is_some_and(|description| description.contains("no knowledge claims"))
            })
    );

    let explanation = reopened
        .request(tool_request(
            3027,
            "explain_answer_graph",
            json!({"query_text": format!("what follows {symbol} and why"), "ctx_id": 0}),
        ))
        .expect("explain zero-claim graph");
    let explanation: Value =
        serde_json::from_str(response_text(&explanation)).expect("explanation JSON");
    assert_eq!(answer["graph"], explanation["graph"]);

    let no_match = reopened
        .request(tool_request(
            3028,
            "query",
            json!({"query_text": "unrelated_symbol_that_is_not_registered", "ctx_id": 0}),
        ))
        .expect("query unrelated symbol");
    let no_match: Value =
        serde_json::from_str(response_text(&no_match)).expect("NoMatch AnswerPack JSON");
    assert_eq!(no_match["status"], "NoMatch");
    assert_eq!(no_match["graph"]["node_count"], 0);
    assert_eq!(no_match["claims"].as_array().map(Vec::len), Some(0));

    let report = reopened.finish();
    assert!(
        !report.timed_out && report.status.success(),
        "reopened symbol-only MCP process failed: {}",
        report.stderr
    );
}

#[test]
fn multi_candidate_answer_graph_is_deterministic_across_process_reopen() {
    let root = TempDir::new().expect("determinism temp root");
    let base_name = "deterministic-candidates";
    let query_text = "deterministic_shared_symbol";
    let first_graph;
    {
        let mut process = McpProcess::spawn(root.path(), base_name);
        process
            .request(codex_initialize_request(4000))
            .expect("initialize first process");
        for (id, suffix) in [(4001, "alpha"), (4002, "beta")] {
            let response = process
                .request(tool_request(
                    id,
                    "ingest",
                    json!({
                        "atom_type": "DECISION",
                        "symbols": [query_text, suffix],
                        "claims": []
                    }),
                ))
                .expect("ingest deterministic candidate");
            assert_success(&response);
        }
        let response = process
            .request(tool_request(
                4003,
                "query",
                json!({"query_text": query_text, "ctx_id": 0}),
            ))
            .expect("first deterministic query");
        let answer: Value = serde_json::from_str(response_text(&response)).unwrap();
        first_graph = answer["graph"].clone();
        assert!(!first_graph["nodes"].as_array().unwrap().is_empty());
        assert!(process.finish().status.success());
    }

    let mut reopened = McpProcess::spawn(root.path(), base_name);
    reopened
        .request(codex_initialize_request(4010))
        .expect("initialize reopened process");
    let response = reopened
        .request(tool_request(
            4011,
            "query",
            json!({"query_text": query_text, "ctx_id": 0}),
        ))
        .expect("reopened deterministic query");
    let answer: Value = serde_json::from_str(response_text(&response)).unwrap();
    assert_eq!(answer["graph"], first_graph);
    assert!(reopened.finish().status.success());
}

fn assert_single_source_graph(graph: &Value, atom_id: &str) {
    assert_eq!(graph["node_count"], 1);
    assert_eq!(graph["evidence_ref_count"], 1);
    assert_eq!(graph["evidence_record_count"], 1);
    assert_eq!(graph["source_link_count"], 1);
    let nodes = graph["nodes"].as_array().expect("AnswerGraph nodes array");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["atom_id"], atom_id);
    assert_eq!(nodes[0]["evidence_ref_count"], 1);
    assert_eq!(nodes[0]["source_link_count"], 1);

    let mut identities = HashSet::new();
    let records = nodes
        .iter()
        .flat_map(|node| node["direct_evidence"].as_array().unwrap())
        .collect::<Vec<_>>();
    for record in &records {
        let legacy = &record["legacy_ref"];
        let identity = format!(
            "{}:{}:{}:{}:{}",
            legacy["atom_id"],
            legacy["section_kind"],
            legacy["offset"],
            legacy["length"],
            record["source_id"]
        );
        assert!(identities.insert(identity), "duplicate evidence identity");
    }
    assert_eq!(records.len(), 1);
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
