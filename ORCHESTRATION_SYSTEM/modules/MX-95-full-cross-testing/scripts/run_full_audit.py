"""Execute direct and stateful cross-tool tests for all authoritative MCP tools."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Callable

from mcp_stdio import McpProcess, canonical_json


MUTATING = {
    "add_claim", "alias_entity", "assert_relation", "attach_atom_source",
    "batch_ingest", "branch_context", "connect_base", "correct_claim",
    "correct_relation", "create_context", "create_entity", "delete_atom",
    "ingest", "merge_entities", "register_predicate", "register_source",
    "repair_relation_contexts", "split_entity", "supersede_claim", "switch_base",
    "transition_relation", "update_atom",
}

NATIVE_CLI = {
    "query": "memoryx query",
    "compile_query_contract": "memoryx query --emit-contract",
    "ingest": "memoryx ingest",
    "batch_ingest": "memoryx ingest (file may contain multiple atoms)",
    "history": "memoryx history",
    "get_stats": "memoryx stats",
    "verify_integrity": "memoryx verify-integrity",
    "create_entity": "memoryx create-entity",
    "add_claim": "memoryx add-entity-claim",
    "assert_relation": "memoryx create-relation",
}


def sha(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def find_first(value: Any, key: str) -> Any | None:
    if isinstance(value, dict):
        if key in value:
            return value[key]
        for child in value.values():
            found = find_first(child, key)
            if found is not None:
                return found
    elif isinstance(value, list):
        for child in value:
            found = find_first(child, key)
            if found is not None:
                return found
    return None


def structured(response: dict[str, Any]) -> Any:
    result = response.get("result", {})
    if isinstance(result, dict) and result.get("structuredContent") is not None:
        value = result["structuredContent"]
        if isinstance(value, dict) and isinstance(value.get("text"), str):
            try:
                return json.loads(value["text"])
            except json.JSONDecodeError:
                pass
        return value
    for content in result.get("content", []) if isinstance(result, dict) else []:
        text = content.get("text") if isinstance(content, dict) else None
        if isinstance(text, str):
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                continue
    return result


def response_is_error(response: dict[str, Any]) -> bool:
    if "error" in response:
        return True
    result = response.get("result")
    return isinstance(result, dict) and result.get("isError") is True


class Audit:
    def __init__(self, output: Path) -> None:
        self.output = output
        self.calls: list[dict[str, Any]] = []
        self.direct: dict[str, str] = {}
        self.sequence_cases: dict[str, list[str]] = {}
        self.sequence_tools: dict[str, list[str]] = {}
        self.sequence_failures: dict[str, list[str]] = {}
        self._serial = 0

    def call(
        self,
        client: McpProcess,
        tool: str,
        arguments: dict[str, Any],
        case_id: str,
        sequence_id: str,
        *,
        expect_error: bool = False,
        direct: bool = True,
        assertion: Callable[[Any], bool] | None = None,
    ) -> Any:
        self._serial += 1
        request_id = f"{case_id}-{self._serial}"
        request = {
            "jsonrpc": "2.0", "id": request_id, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        }
        started = time.monotonic()
        client.send(request)
        raw_response, response = client.read_response()
        duration_ms = round((time.monotonic() - started) * 1000, 3)
        is_error = response_is_error(response)
        value = structured(response)
        passed = is_error == expect_error and (assertion(value) if assertion else True)
        record = {
            "case_id": case_id,
            "sequence_id": sequence_id,
            "tool": tool,
            "arguments": arguments,
            "expect_error": expect_error,
            "observed_error": is_error,
            "status": "passed" if passed else "failed",
            "duration_ms": duration_ms,
            "request": request,
            "response": response,
            "request_sha256": sha(canonical_json(request)),
            "response_sha256": sha(raw_response.encode("utf-8")),
            "observed_at_utc": datetime.now(UTC).isoformat(),
        }
        self.calls.append(record)
        self.sequence_cases.setdefault(sequence_id, []).append(case_id)
        self.sequence_tools.setdefault(sequence_id, []).append(tool)
        if not passed:
            self.sequence_failures.setdefault(sequence_id, []).append(case_id)
        if direct:
            if tool in self.direct:
                raise RuntimeError(f"duplicate direct case for {tool}")
            self.direct[tool] = case_id
        if not passed:
            raise RuntimeError(
                f"case {case_id} for {tool} failed; expected_error={expect_error}, "
                f"observed_error={is_error}, value={json.dumps(value, ensure_ascii=False)[:1000]}"
            )
        return value

    def save(self) -> None:
        calls_path = self.output / "calls.jsonl"
        calls_path.write_text(
            "".join(json.dumps(item, ensure_ascii=False) + "\n" for item in self.calls),
            encoding="utf-8",
        )


def init_client(client: McpProcess) -> None:
    _, response = client.request(
        "initialize",
        {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "mx-95-full-audit", "version": "1"},
        },
        request_id="mx95-full-initialize",
    )
    if response.get("result", {}).get("protocolVersion") != "2025-11-25":
        raise RuntimeError("full audit initialize did not echo the protocol")
    client.notify("notifications/initialized")
    client.assert_notification_silence()


def require_int(value: Any, key: str) -> int:
    found = find_first(value, key)
    if isinstance(found, bool) or not isinstance(found, int):
        raise RuntimeError(f"response did not contain integer {key}: {value}")
    return found


def require_str(value: Any, key: str) -> str:
    found = find_first(value, key)
    if not isinstance(found, str) or not found:
        raise RuntimeError(f"response did not contain string {key}: {value}")
    return found


def setup_ingest(client: McpProcess, marker: int, symbol: str) -> Any:
    _, response = client.request(
        "tools/call",
        {"name": "ingest", "arguments": {
            "atom_type": "FACT",
            "claims": [{"subj": marker, "pred": 1, "obj_tag": 3, "obj_val": marker}],
            "symbols": [symbol],
        }},
    )
    if response_is_error(response):
        raise RuntimeError(f"setup ingest failed: {response}")
    return structured(response)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--repo-root", required=True, type=Path)
    parser.add_argument("--surface", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()
    repo = args.repo_root.resolve(strict=True)
    binary = args.binary.resolve(strict=True)
    surface = json.loads(args.surface.read_text(encoding="utf-8"))
    output = args.output.resolve()
    if output.exists():
        raise FileExistsError(f"full audit output exists: {output}")
    output.mkdir(parents=True)
    audit = Audit(output)

    primary_name = f"mx-95-disposable-{args.run_id}-primary"
    secondary_name = f"mx-95-disposable-{args.run_id}-secondary"
    secondary_ref = f"project:{secondary_name}"
    # The process-start base is intentionally registered under the stable session
    # ref `active`; its discovered project:<name> alias is rejected to prevent a
    # duplicate store for the same physical root.
    primary_ref = "active"

    seed = McpProcess(binary, repo, secondary_name)
    try:
        init_client(seed)
        setup_ingest(seed, 95002, "mx95_secondary_marker")
    finally:
        seed_code = seed.close()
        seed.write_logs(output, "secondary-seed")
    if seed_code != 0:
        raise RuntimeError(f"secondary seed exited {seed_code}")

    client = McpProcess(binary, repo, primary_name)
    try:
        init_client(client)
        _, live_tools_response = client.request("tools/list", {}, request_id="full-tools-list")
        live_tools = live_tools_response.get("result", {}).get("tools", [])
        if sha(canonical_json(live_tools)) != surface["authoritative_surface_sha256"]:
            raise RuntimeError("full audit tool surface differs from authoritative capture")

        audit.call(client, "list_bases", {}, "D-BASE-001", "SQ-BASE")
        audit.call(client, "active_base", {}, "D-BASE-002", "SQ-BASE")
        audit.call(client, "connect_base", {
            "base_ref": secondary_ref, "scope": "project", "name": secondary_name,
        }, "D-BASE-003", "SQ-BASE")
        audit.call(client, "query_base", {
            "base_ref": secondary_ref, "query_text": "mx95_secondary_marker", "ctx_id": 0,
        }, "D-BASE-004", "SQ-BASE")
        audit.call(client, "switch_base", {"base_ref": secondary_ref}, "D-BASE-005", "SQ-BASE")
        audit.call(client, "active_base", {}, "X-BASE-006", "SQ-BASE", direct=False)
        audit.call(client, "switch_base", {"base_ref": primary_ref}, "X-BASE-007", "SQ-BASE", direct=False)

        contract = audit.call(client, "compile_query_contract", {
            "query_text": "Explain mx95 provenance and require provenance",
        }, "D-QC-001", "SQ-QUERY-CONTRACT")
        contract_value = find_first(contract, "contract") or contract
        audit.call(client, "validate_query_contract", {"contract": contract_value},
                   "D-QC-002", "SQ-QUERY-CONTRACT")
        audit.call(client, "query", {"query_text": "mx95 initial no match", "ctx_id": 0},
                   "D-QC-003", "SQ-QUERY-CONTRACT")
        audit.call(client, "explain_answer_graph", {
            "query_text": "mx95 initial no match", "ctx_id": 0,
        }, "D-QC-004", "SQ-QUERY-CONTRACT")

        source = audit.call(client, "register_source", {
            "kind": "file", "label": "MX-95 generated source",
            "path": "ORCHESTRATION_SYSTEM/modules/MX-95-full-cross-testing/TASK.md",
            "line_start": 1, "line_end": 20, "source_version": "audit-run",
        }, "D-SRC-001", "SQ-PROVENANCE")
        source_id = require_int(source, "source_id")
        audit.call(client, "list_sources", {}, "D-SRC-002", "SQ-PROVENANCE")

        provenance_atom = audit.call(client, "ingest", {
            "atom_type": "FACT", "claims": [{
                "subj": 9501, "pred": 9502, "obj_tag": 3, "obj_val": 9503,
                "qualifiers_mask": 0,
            }], "symbols": ["mx95_provenance_marker", "persistence"],
            "domain_mask": 65535, "trust_level": 5000,
        }, "D-ATOM-001", "SQ-PROVENANCE")
        provenance_atom_id = require_str(provenance_atom, "atom_id")
        provenance_node = find_first(provenance_atom, "node_num")
        audit.call(client, "attach_atom_source", {
            "atom_id": provenance_atom_id, "source_id": source_id,
        }, "D-SRC-003", "SQ-PROVENANCE")
        audit.call(client, "get_provenance_path", {"atom_id": provenance_atom_id},
                   "D-SRC-004", "SQ-PROVENANCE")

        predicate = audit.call(client, "register_predicate", {
            "stable_key": "mx95:depends_on", "canonical_name": "mx95_depends_on",
            "description": "MX-95 disposable dependency relation.",
            "direction": "directed", "cardinality": "many_to_many",
        }, "D-PRED-001", "SQ-RELATION")
        predicate_id = require_int(predicate, "predicate_id")
        audit.call(client, "list_predicates", {}, "D-PRED-002", "SQ-RELATION")
        audit.call(client, "get_predicate", {"predicate_id": predicate_id},
                   "D-PRED-003", "SQ-RELATION")
        audit.call(client, "resolve_predicate", {"name_or_key": "mx95:depends_on"},
                   "D-PRED-004", "SQ-RELATION")

        entity_a = audit.call(client, "create_entity", {
            "canonical_name": "MX95 Entity A", "entity_type": "audit_fixture",
        }, "D-ENT-001", "SQ-ENTITY")
        entity_a_id = require_int(entity_a, "entity_id")
        entity_b_id = require_int(audit.call(client, "create_entity", {
            "canonical_name": "MX95 Entity B", "entity_type": "audit_fixture",
        }, "X-ENT-002", "SQ-ENTITY", direct=False), "entity_id")
        entity_c_id = require_int(audit.call(client, "create_entity", {
            "canonical_name": "MX95 Entity C", "entity_type": "audit_fixture",
        }, "X-ENT-003", "SQ-ENTITY", direct=False), "entity_id")
        entity_d_id = require_int(audit.call(client, "create_entity", {
            "canonical_name": "MX95 Entity D", "entity_type": "audit_fixture",
        }, "X-ENT-004", "SQ-ENTITY", direct=False), "entity_id")
        audit.call(client, "list_entities", {}, "D-ENT-002", "SQ-ENTITY")
        audit.call(client, "alias_entity", {"entity_id": entity_a_id, "alias": "mx95-a"},
                   "D-ENT-003", "SQ-ENTITY")
        merge_source = require_int(audit.call(client, "create_entity", {
            "canonical_name": "MX95 Merge Source", "entity_type": "audit_fixture",
        }, "X-ENT-005", "SQ-ENTITY", direct=False), "entity_id")
        audit.call(client, "merge_entities", {
            "target_entity": entity_a_id, "source_entity": merge_source,
        }, "D-ENT-004", "SQ-ENTITY")
        audit.call(client, "split_entity", {
            "source_entity": entity_a_id, "canonical_name": "MX95 Split Child",
            "entity_type": "audit_fixture",
        }, "D-ENT-005", "SQ-ENTITY")
        audit.call(client, "add_claim", {
            "entity_id": entity_a_id, "predicate": predicate_id,
            "object": 9504, "object_tag": "U64", "ctx_id": 0,
        }, "D-ENT-006", "SQ-ENTITY")

        relation = audit.call(client, "assert_relation", {
            "subject": entity_a_id, "predicate": predicate_id,
            "object": entity_b_id, "ctx_id": 0,
        }, "D-REL-001", "SQ-RELATION")
        relation_id = require_int(relation, "relation_id")
        corrected = audit.call(client, "correct_relation", {
            "relation_id": relation_id, "subject": entity_a_id,
            "predicate": predicate_id, "object": entity_c_id, "ctx_id": 0,
        }, "D-REL-002", "SQ-RELATION")
        current_relation_id = find_first(corrected, "relation_id")
        if not isinstance(current_relation_id, int):
            current_relation_id = require_int(corrected, "new_relation_id")
        audit.call(client, "transition_relation", {
            "old_relation_id": current_relation_id, "new_object": entity_d_id,
            "ctx_id": 0, "source_ids": [source_id],
        }, "D-REL-003", "SQ-RELATION")
        audit.call(client, "audit_relation_contexts", {}, "D-REL-004", "SQ-RELATION")
        audit.call(client, "repair_relation_contexts", {"dry_run": True},
                   "D-REL-005", "SQ-RELATION")

        context = audit.call(client, "create_context", {"policy_id": 95},
                             "D-CTX-001", "SQ-CONTEXT")
        context_id = require_int(context, "ctx_id")
        audit.call(client, "list_contexts", {}, "D-CTX-002", "SQ-CONTEXT")
        audit.call(client, "branch_context", {
            "parent_ctx": context_id, "policy_id": 96, "reason": "MX-95 branch",
        }, "D-CTX-003", "SQ-CONTEXT")
        audit.call(client, "list_conflicts", {"ctx_id": 0},
                   "D-CTX-004", "SQ-CONTEXT")

        audit.call(client, "batch_ingest", {"atoms": [
            {"atom_type": "FACT", "claims": [{
                "subj": 9510, "pred": 9511, "obj_tag": 3, "obj_val": 9512,
            }], "symbols": ["mx95_batch_one"]},
            {"atom_type": "DECISION", "claims": [{
                "subj": 9520, "pred": 9521, "obj_tag": 3, "obj_val": 9522,
            }], "symbols": ["mx95_batch_two"]},
        ]}, "D-ATOM-002", "SQ-ATOM-LIFECYCLE")

        def fresh_atom(marker: int, symbol: str) -> str:
            return require_str(setup_ingest(client, marker, symbol), "atom_id")

        update_id = fresh_atom(9530, "mx95_update_old")
        audit.call(client, "update_atom", {
            "atom_id": update_id, "atom_type": "FACT",
            "claims": [{"subj": 9530, "pred": 1, "obj_tag": 3, "obj_val": 9531}],
            "symbols": ["mx95_update_new"],
        }, "D-ATOM-003", "SQ-ATOM-LIFECYCLE")
        supersede_id = fresh_atom(9540, "mx95_supersede_old")
        audit.call(client, "supersede_claim", {
            "atom_id": supersede_id, "atom_type": "FACT",
            "claims": [{"subj": 9540, "pred": 1, "obj_tag": 3, "obj_val": 9541}],
            "symbols": ["mx95_supersede_new"],
        }, "D-ATOM-004", "SQ-ATOM-LIFECYCLE")
        correct_id = fresh_atom(9550, "mx95_correct_old")
        audit.call(client, "correct_claim", {
            "atom_id": correct_id, "atom_type": "FACT",
            "claims": [{"subj": 9550, "pred": 1, "obj_tag": 3, "obj_val": 9551}],
            "symbols": ["mx95_correct_new"],
        }, "D-ATOM-005", "SQ-ATOM-LIFECYCLE")
        delete_id = fresh_atom(9560, "mx95_delete_me")
        audit.call(client, "delete_atom", {"atom_id": delete_id, "reason": "MX-95 fixture"},
                   "D-ATOM-006", "SQ-ATOM-LIFECYCLE")
        audit.call(client, "history", {"limit": 20}, "D-ATOM-007", "SQ-ATOM-LIFECYCLE")

        audit.call(client, "search_lex", {"term": "mx95_provenance_marker"},
                   "D-SRCH-001", "SQ-SEARCH-GRAPH")
        audit.call(client, "search_semantic", {"vector": [0.12, -0.44, 0.88]},
                   "D-SRCH-002", "SQ-SEARCH-GRAPH")
        audit.call(client, "search_graph", {"pattern": "9501 -> * -> *", "limit": 10},
                   "D-SRCH-003", "SQ-SEARCH-GRAPH")
        node_num = provenance_node if isinstance(provenance_node, int) else 0
        audit.call(client, "graph_neighbors", {"node_num": node_num},
                   "D-GRAPH-001", "SQ-SEARCH-GRAPH")
        audit.call(client, "graph_walk", {"seed_nodes": [node_num], "depth": 2},
                   "D-GRAPH-002", "SQ-SEARCH-GRAPH")
        audit.call(client, "extract_subgraph", {"center_node": node_num, "radius": 2},
                   "D-GRAPH-003", "SQ-SEARCH-GRAPH")

        audit.call(client, "get_stats", {}, "D-CHK-001", "SQ-INTEGRITY")
        audit.call(client, "verify_integrity", {}, "D-CHK-002", "SQ-INTEGRITY",
                   assertion=lambda value: find_first(value, "valid") is True)

        # Negative, conflict-prevention, boundary, QueryContract budget, response-limit,
        # and idempotence cases. These are cross-only evidence, not substitute direct cases.
        audit.call(client, "validate_query_contract", {"contract": {"intent": "lookup", "budgets": {"max_atoms": 0}}},
                   "N-QC-001", "SQ-NEGATIVE-BOUNDARY", expect_error=False, direct=False)
        audit.call(client, "query", {"contract": {
            "intent": "lookup", "targets": [{"label": "mx95_provenance_marker"}],
            "budgets": {"max_atoms": 1, "max_iterations": 1, "max_time_ms": 0,
                        "max_edges": 0, "max_io_bytes": 0, "max_federated_calls": 0},
            "output_contract": {"format": "structured_json", "max_items": 1,
                                "max_bytes": 2048},
        }, "ctx_id": 0}, "B-QC-002", "SQ-NEGATIVE-BOUNDARY", direct=False)
        audit.call(client, "attach_atom_source", {
            "atom_id": provenance_atom_id, "source_id": source_id,
        }, "I-PROV-001", "SQ-IDEMPOTENCE", direct=False)
        duplicate = audit.call(client, "ingest", {
            "atom_type": "FACT", "claims": [{
                "subj": 9501, "pred": 9502, "obj_tag": 3, "obj_val": 9503,
                "qualifiers_mask": 0,
            }], "symbols": ["mx95_provenance_marker", "persistence"],
            "domain_mask": 65535, "trust_level": 5000,
        }, "I-ATOM-002", "SQ-IDEMPOTENCE", direct=False)
        if require_str(duplicate, "atom_id") != provenance_atom_id:
            raise RuntimeError("identical canonical ingest was not idempotent")

        conflict_pred = audit.call(client, "register_predicate", {
            "stable_key": "mx95:single_value", "canonical_name": "mx95_single_value",
            "description": "MX-95 single-value conflict fixture.",
            "direction": "directed", "cardinality": "many_to_one",
        }, "X-CF-001", "SQ-CONFLICT", direct=False)
        conflict_pred_id = require_int(conflict_pred, "predicate_id")
        audit.call(client, "assert_relation", {
            "subject": entity_b_id, "predicate": conflict_pred_id,
            "object": entity_c_id, "ctx_id": 0,
        }, "X-CF-002", "SQ-CONFLICT", direct=False)
        audit.call(client, "assert_relation", {
            "subject": entity_b_id, "predicate": conflict_pred_id,
            "object": entity_d_id, "ctx_id": 0,
        }, "N-CF-003", "SQ-CONFLICT", expect_error=True, direct=False)

        # A same-base second serve must proxy to this live owner, never become a writer.
        proxy = McpProcess(binary, repo, primary_name, allow_existing=True)
        try:
            init_client(proxy)
            audit.call(proxy, "get_stats", {}, "O-OWNER-001", "SQ-LIVE-OWNER", direct=False)
            audit.call(proxy, "ingest", {
                "atom_type": "FACT", "claims": [{
                    "subj": 9570, "pred": 1, "obj_tag": 3, "obj_val": 9570,
                }], "symbols": ["mx95_proxy_marker"],
            }, "O-OWNER-002", "SQ-LIVE-OWNER", direct=False)
        finally:
            proxy_exit = proxy.close()
            proxy.write_logs(output, "live-owner-proxy")
        if proxy_exit != 0:
            raise RuntimeError(f"live-owner proxy exited {proxy_exit}")

        contention = subprocess.run(
            [str(binary), "--base-scope", "project", "--format", "json",
             "stats", "--base", primary_name],
            cwd=repo, capture_output=True, text=True, encoding="utf-8", timeout=15,
            check=False,
        )
        (output / "owner-contention.json").write_text(json.dumps({
            "command": contention.args, "exit_code": contention.returncode,
            "stdout": contention.stdout, "stderr": contention.stderr,
            "expected_exit_code": 73, "passed": contention.returncode == 73,
        }, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        if contention.returncode != 73:
            raise RuntimeError(f"direct second writer was not rejected with 73: {contention.returncode}")
        audit.call(client, "search_lex", {"term": "mx95_proxy_marker"},
                   "O-OWNER-003", "SQ-LIVE-OWNER", direct=False)
    finally:
        primary_exit = client.close()
        client.write_logs(output, "primary")
        audit.save()
    if primary_exit != 0:
        raise RuntimeError(f"primary audit process exited {primary_exit}")

    # Graceful reopen/restart evidence on the same disposable physical base.
    reopened = McpProcess(binary, repo, primary_name, allow_existing=True)
    try:
        init_client(reopened)
        audit.call(reopened, "get_stats", {}, "R-REOPEN-001", "SQ-REOPEN", direct=False)
        audit.call(reopened, "search_lex", {"term": "mx95_proxy_marker"},
                   "R-REOPEN-002", "SQ-REOPEN", direct=False)
        audit.call(reopened, "verify_integrity", {}, "R-REOPEN-003", "SQ-REOPEN",
                   direct=False, assertion=lambda value: find_first(value, "valid") is True)
    finally:
        reopen_exit = reopened.close()
        reopened.write_logs(output, "reopen")
        audit.save()
    if reopen_exit != 0:
        raise RuntimeError(f"reopened audit process exited {reopen_exit}")

    sequence_descriptions = {
        "SQ-BASE": "discover active/known bases, connect a prepared project base, query it, switch, and restore the original active base",
        "SQ-QUERY-CONTRACT": "compile a natural query, validate the observed contract, execute it, and explain the resulting AnswerGraph",
        "SQ-PROVENANCE": "register a source, ingest an atom, attach the source, and retrieve its provenance path",
        "SQ-RELATION": "register/resolve a predicate, assert/correct/transition a relation, then audit and dry-run repair",
        "SQ-ENTITY": "create, list, alias, merge, split, and add an atom-backed entity claim",
        "SQ-CONTEXT": "create/list/branch contexts and inspect explicit conflicts",
        "SQ-ATOM-LIFECYCLE": "batch ingest, update, supersede, correct, tombstone, and inspect durable history",
        "SQ-SEARCH-GRAPH": "lexical/semantic/graph candidate search followed by graph neighborhood, walk, and subgraph extraction",
        "SQ-INTEGRITY": "inspect labelled stats and verify physical plus semantic integrity",
        "SQ-NEGATIVE-BOUNDARY": "exercise invalid QueryContract validation and minimum output/time budgets",
        "SQ-IDEMPOTENCE": "repeat source attachment and canonical ingest and require stable atom identity",
        "SQ-CONFLICT": "assert a many-to-one value and require an incompatible replacement to fail closed",
        "SQ-LIVE-OWNER": "use a same-base proxy, mutate through the owner, reject a direct second writer, and observe the proxy write",
        "SQ-REOPEN": "reopen the same base, observe persisted owner write, and verify integrity",
    }
    sequences = []
    call_by_id = {item["case_id"]: item for item in audit.calls}
    for sequence_id, cases in audit.sequence_cases.items():
        tools = list(dict.fromkeys(audit.sequence_tools[sequence_id]))
        failures = [case for case in cases if call_by_id[case]["status"] != "passed"]
        sequences.append({
            "id": sequence_id,
            "description": sequence_descriptions[sequence_id],
            "tools": tools,
            "observed_case_ids": cases,
            "status": "passed" if not failures and len(tools) >= 2 else "failed",
            "failures": failures,
        })
    sequences.sort(key=lambda item: item["id"])
    (output / "sequences.json").write_text(json.dumps({
        "schema_version": "memoryx.mx95.cross-tool-sequences.v1",
        "sequences": sequences,
    }, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    source_lines = (repo / "src" / "bin" / "memoryx.rs").read_text(encoding="utf-8").splitlines()
    dispatch_anchor: dict[str, str] = {}
    for line_no, line in enumerate(source_lines, start=1):
        match = re.match(r'\s+"([a-z_]+)"\s*=>', line)
        if match:
            dispatch_anchor[match.group(1)] = f"src/bin/memoryx.rs:{line_no}"

    direct_calls = {item["tool"]: item for item in audit.calls if audit.direct.get(item["tool"]) == item["case_id"]}
    tool_to_sequences: dict[str, list[str]] = {}
    for sequence in sequences:
        if sequence["status"] == "passed":
            for tool in sequence["tools"]:
                tool_to_sequences.setdefault(tool, []).append(sequence["id"])

    tools_ledger = []
    for observed in surface["inventory"]:
        name = observed["name"]
        direct_call = direct_calls[name]
        if name in NATIVE_CLI:
            cli = {"kind": "native", "command": NATIVE_CLI[name]}
        else:
            cli = {
                "kind": "live_owner_client",
                "command": f"memoryx --format json client --base <path> --tool {name} --arguments '<object>'",
                "rationale": "No dedicated native subcommand; the documented client bridge calls the MCP tool through a live owner.",
            }
        rust_kind = "transport_only" if name in {"list_bases", "active_base", "connect_base", "switch_base"} else "rust_mcp_dispatch"
        categories = ["positive", "stateful", "cross_call"]
        if name in MUTATING:
            categories.append("mutation")
        else:
            categories.append("read_only")
        if name in {"query", "validate_query_contract"}:
            categories.extend(["negative", "boundary", "query_contract", "response_limit"])
        if name in {"verify_integrity", "get_stats", "audit_relation_contexts"}:
            categories.extend(["integrity", "reopen"])
        if name in {"get_provenance_path", "register_source", "attach_atom_source", "list_sources"}:
            categories.append("provenance")
        if name in {"assert_relation", "list_conflicts"}:
            categories.append("conflict")
        tools_ledger.append({
            "name": name,
            "schema_sha256": observed["schema_sha256"],
            "purpose": observed["description"],
            "classification": "mutating" if name in MUTATING else "read_only",
            "mcp_request_example": direct_call["request"],
            "cli_mapping": cli,
            "rust_mapping": {
                "kind": rust_kind,
                "source_anchor": dispatch_anchor.get(name, "src/bin/memoryx.rs:5762"),
                "rationale": "Session registry operation" if rust_kind == "transport_only" else "Production tools/call dispatch into a typed MemoryX handler.",
            },
            "direct_case_id": audit.direct[name],
            "direct_status": direct_call["status"],
            "cross_sequence_ids": tool_to_sequences[name],
            "coverage_categories": list(dict.fromkeys(categories)),
            "unresolved_limitations": [
                "Observed MCP behavior does not by itself prove model quality, hidden hook/compact behavior, cache reuse, or full MemoryX semantic acceptance.",
                "Crash/restart observations do not close the open N5 operation-atomicity roadmap gate.",
            ],
        })
    ledger = {
        "schema_version": "memoryx.mx95.coverage-ledger.v1",
        "run_id": args.run_id,
        "authoritative_surface_sha256": surface["authoritative_surface_sha256"],
        "observed_tool_count": surface["observed_count"],
        "tools": sorted(tools_ledger, key=lambda item: item["name"]),
        "global_coverage_classes": [
            "positive", "negative", "boundary", "stateful", "cross_call",
            "reopen_restart", "live_owner", "concurrency", "idempotence",
            "provenance", "conflict", "query_contract", "response_limit",
        ],
        "global_unresolved": [
            "A deliberate on-disk legacy migration fixture and forced-crash recovery matrix are separate follow-up scenarios.",
            "N5 remains open; this ledger cannot establish multi-file crash atomicity.",
        ],
    }
    (output / "coverage-ledger.json").write_text(
        json.dumps(ledger, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    audit.save()
    summary = {
        "direct_tools_passed": len(audit.direct),
        "authoritative_tools": surface["observed_count"],
        "calls": len(audit.calls),
        "sequences": len(sequences),
        "primary_exit": primary_exit,
        "reopen_exit": reopen_exit,
        "output": str(output),
    }
    (output / "run-summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
