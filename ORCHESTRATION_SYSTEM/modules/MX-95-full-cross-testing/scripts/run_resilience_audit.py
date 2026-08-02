"""MX-95 restart, determinism, response-limit, migration, and crash scenarios."""

from __future__ import annotations

import argparse
import hashlib
import json
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from mcp_stdio import McpProcess, canonical_json
from run_full_audit import Audit, find_first, init_client, require_int, require_str, setup_ingest


def sha_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def case(cases: list[dict[str, Any]], name: str, passed: bool, evidence: Any) -> None:
    cases.append({"name": name, "passed": passed, "evidence": evidence})
    if not passed:
        raise RuntimeError(f"resilience case failed: {name}: {evidence}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--repo-root", required=True, type=Path)
    parser.add_argument("--run-root", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--primary-name", required=True)
    parser.add_argument("--core-attempt", required=True)
    parser.add_argument("--attempt", required=True, type=int)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    repo = args.repo_root.resolve(strict=True)
    run_root = args.run_root.resolve(strict=True)
    attempt_suffix = f"r{args.attempt:02d}"
    output = run_root / f"resilience-attempt-{args.attempt:02d}"
    if output.exists():
        raise FileExistsError(f"resilience output exists: {output}")
    output.mkdir()
    audit = Audit(output)
    cases: list[dict[str, Any]] = []

    # Confirm that the earlier direct provenance result contains the registered
    # source and exact module-owned location, rather than merely a non-error frame.
    core_root = run_root / args.core_attempt
    full_calls_path = core_root / "calls.jsonl"
    full_calls = [json.loads(line) for line in full_calls_path.read_text(encoding="utf-8").splitlines() if line]
    provenance_call = next(item for item in full_calls if item["case_id"] == "D-SRC-004")
    provenance_text = provenance_call["response"]["result"]["structuredContent"]["text"]
    provenance = json.loads(provenance_text)
    provenance_ok = (
        find_first(provenance, "source_id") == 1
        and find_first(provenance, "path")
        == "ORCHESTRATION_SYSTEM/modules/MX-95-full-cross-testing/TASK.md"
    )
    case(cases, "source_backed_provenance", provenance_ok, {
        "source_id": find_first(provenance, "source_id"),
        "path": find_first(provenance, "path"),
        "direct_case": "D-SRC-004",
    })

    # Query determinism and output-contract enforcement, including a complete
    # graceful reopen between equivalent requests.
    first = McpProcess(binary, repo, args.primary_name, allow_existing=True)
    try:
        init_client(first)
        q1 = audit.call(first, "query", {"query_text": "mx95_provenance_marker", "ctx_id": 0},
                        "E-DET-001", "SQ-DETERMINISM", direct=False)
        q2 = audit.call(first, "query", {"query_text": "mx95_provenance_marker", "ctx_id": 0},
                        "E-DET-002", "SQ-DETERMINISM", direct=False)
        case(cases, "same_process_deterministic_query", q1 == q2, {
            "first_sha256": hashlib.sha256(canonical_json(q1)).hexdigest(),
            "second_sha256": hashlib.sha256(canonical_json(q2)).hexdigest(),
        })
        compiled = audit.call(first, "compile_query_contract", {
            "query_text": "mx95_provenance_marker",
        }, "E-LIMIT-001", "SQ-RESPONSE-LIMIT", direct=False)
        contract = find_first(compiled, "contract") or compiled
        contract["budgets"] = {
            "max_atoms": 64, "max_iterations": 4, "max_time_ms": 30000,
            "max_edges": 1024, "max_io_bytes": 1048576, "max_federated_calls": 0,
        }
        contract["output_contract"] = {
            "format": "structured_json", "include_answer_graph": True,
            "include_confidence": True, "include_execution_trace": True,
            "include_provenance": True, "max_items": 1, "max_bytes": 2048,
        }
        limited = audit.call(first, "query", {"contract": contract, "ctx_id": 0},
                             "E-LIMIT-002", "SQ-RESPONSE-LIMIT", direct=False)
        limits = find_first(limited, "response_limits")
        limit_ok = (
            isinstance(limits, dict)
            and limits.get("max_bytes") == 2048
            and limits.get("max_items") == 1
            and limits.get("emitted_bytes", 10**9) <= 2048
            and (limits.get("bytes_truncated") is True or limits.get("items_truncated") is True)
        )
        case(cases, "query_response_limit_enforced", limit_ok, limits)
    finally:
        first_exit = first.close()
        first.write_logs(output, "determinism-first")
    case(cases, "determinism_first_clean_exit", first_exit == 0, {"exit_code": first_exit})

    second = McpProcess(binary, repo, args.primary_name, allow_existing=True)
    try:
        init_client(second)
        q3 = audit.call(second, "query", {"query_text": "mx95_provenance_marker", "ctx_id": 0},
                        "E-DET-003", "SQ-DETERMINISM", direct=False)
        verify = audit.call(second, "verify_integrity", {}, "E-DET-004", "SQ-DETERMINISM",
                            direct=False)
        case(cases, "cross_reopen_deterministic_query", q1 == q3, {
            "before_sha256": hashlib.sha256(canonical_json(q1)).hexdigest(),
            "after_sha256": hashlib.sha256(canonical_json(q3)).hexdigest(),
        })
        case(cases, "clean_base_integrity_after_reopen", find_first(verify, "valid") is True, verify)
    finally:
        second_exit = second.close()
        second.write_logs(output, "determinism-reopen")
    case(cases, "determinism_reopen_clean_exit", second_exit == 0, {"exit_code": second_exit})

    # Build a disposable current relation, remove only its context projection on
    # disk, and verify explicit dry-run/apply/idempotent migration behavior.
    migration_name = f"mx-95-disposable-{args.run_id}-{attempt_suffix}-migration"
    migration = McpProcess(binary, repo, migration_name)
    try:
        init_client(migration)
        pred = audit.call(migration, "register_predicate", {
            "stable_key": "mx95:migration_relation", "canonical_name": "mx95_migration_relation",
            "description": "Disposable MX-95 migration fixture.",
            "direction": "directed", "cardinality": "many_to_many",
        }, "E-MIG-001", "SQ-MIGRATION", direct=False)
        pred_id = require_int(pred, "predicate_id")
        left = require_int(audit.call(migration, "create_entity", {
            "canonical_name": "MX95 Migration Left", "entity_type": "fixture",
        }, "E-MIG-002", "SQ-MIGRATION", direct=False), "entity_id")
        right = require_int(audit.call(migration, "create_entity", {
            "canonical_name": "MX95 Migration Right", "entity_type": "fixture",
        }, "E-MIG-003", "SQ-MIGRATION", direct=False), "entity_id")
        relation = audit.call(migration, "assert_relation", {
            "subject": left, "predicate": pred_id, "object": right, "ctx_id": 0,
        }, "E-MIG-004", "SQ-MIGRATION", direct=False)
        relation_id = require_int(relation, "relation_id")
    finally:
        migration_seed_exit = migration.close()
        migration.write_logs(output, "migration-seed")
    case(cases, "migration_seed_clean_exit", migration_seed_exit == 0,
         {"exit_code": migration_seed_exit, "relation_id": relation_id})

    contexts_path = repo / ".memoryx" / "bases" / migration_name / "meta" / "contexts.json"
    before_contexts = contexts_path.read_text(encoding="utf-8")
    contexts = json.loads(before_contexts)
    active_claims = contexts["contexts"][0]["active_claims"]
    removed_count = len(active_claims)
    if removed_count != 1:
        raise RuntimeError(f"migration fixture expected one active claim, got {removed_count}")
    (output / "migration-contexts.before.json").write_text(before_contexts, encoding="utf-8")
    contexts["contexts"][0]["active_claims"] = {}
    contexts_path.write_text(json.dumps(contexts, separators=(",", ":")), encoding="utf-8")
    damaged_contexts_sha256 = sha_file(contexts_path)
    (output / "migration-contexts.damaged.json").write_text(
        contexts_path.read_text(encoding="utf-8"), encoding="utf-8"
    )

    migration_check = McpProcess(binary, repo, migration_name, allow_existing=True)
    try:
        init_client(migration_check)
        audit_before = audit.call(migration_check, "audit_relation_contexts", {},
                                  "E-MIG-005", "SQ-MIGRATION", direct=False)
        dry = audit.call(migration_check, "repair_relation_contexts", {"dry_run": True},
                         "E-MIG-006", "SQ-MIGRATION", direct=False)
        applied = audit.call(migration_check, "repair_relation_contexts", {
            "relation_ids": [relation_id],
        }, "E-MIG-007", "SQ-MIGRATION", direct=False)
        final_audit = audit.call(migration_check, "audit_relation_contexts", {},
                                 "E-MIG-008", "SQ-MIGRATION", direct=False)
        repeated = audit.call(migration_check, "repair_relation_contexts", {
            "relation_ids": [relation_id],
        }, "E-MIG-009", "SQ-MIGRATION", direct=False)
        migration_ok = (
            find_first(audit_before, "consistent") is False
            and relation_id in (find_first(dry, "eligible_relation_ids") or [])
            and find_first(applied, "mutated") is True
            and find_first(final_audit, "consistent") is True
            and find_first(repeated, "mutated") is False
        )
        case(cases, "relation_context_migration_and_idempotence", migration_ok, {
            "before_issue_count": find_first(audit_before, "issue_count"),
            "eligible_relation_ids": find_first(dry, "eligible_relation_ids"),
            "applied_mutated": find_first(applied, "mutated"),
            "final_consistent": find_first(final_audit, "consistent"),
            "repeat_mutated": find_first(repeated, "mutated"),
            "fixture_before_sha256": hashlib.sha256(before_contexts.encode()).hexdigest(),
            "fixture_damaged_sha256": damaged_contexts_sha256,
        })
    finally:
        migration_exit = migration_check.close()
        migration_check.write_logs(output, "migration-apply")
    case(cases, "migration_process_clean_exit", migration_exit == 0, {"exit_code": migration_exit})

    # Structural process-death recovery: kill only the child created here after a
    # committed response, then require lease release, persisted lookup, and clean
    # integrity on reopen. This deliberately does not inject at N5 boundaries.
    crash_name = f"mx-95-disposable-{args.run_id}-{attempt_suffix}-crash"
    crash = McpProcess(binary, repo, crash_name)
    init_client(crash)
    committed = setup_ingest(crash, 9595, "mx95_committed_before_crash")
    committed_atom = require_str(committed, "atom_id")
    crash_pid = crash.process.pid
    crash.process.kill()
    crash_exit = crash.process.wait(timeout=5)
    crash.write_logs(output, "crash-owner")
    case(cases, "owned_process_forced_exit", crash.process.poll() is not None, {
        "child_pid": crash_pid, "exit_code": crash_exit, "committed_atom": committed_atom,
        "foreign_process_action": "none",
    })

    recovered = McpProcess(binary, repo, crash_name, allow_existing=True)
    try:
        init_client(recovered)
        found = audit.call(recovered, "search_lex", {"term": "mx95_committed_before_crash"},
                           "E-CRASH-001", "SQ-CRASH-RECOVERY", direct=False)
        integrity = audit.call(recovered, "verify_integrity", {},
                               "E-CRASH-002", "SQ-CRASH-RECOVERY", direct=False)
        lookup_count_one = "count=1" in json.dumps(found)
        case(cases, "committed_state_recovered_after_process_death", (
            lookup_count_one
            and find_first(integrity, "valid") is True
            and find_first(integrity, "checked_atoms") == 1
        ), {
            "atom_id": committed_atom,
            "lookup_count_one": lookup_count_one,
            "integrity_valid": find_first(integrity, "valid"),
            "checked_atoms": find_first(integrity, "checked_atoms"),
        })
    finally:
        recovered_exit = recovered.close()
        recovered.write_logs(output, "crash-reopen")
    case(cases, "crash_recovery_clean_exit", recovered_exit == 0,
         {"exit_code": recovered_exit})

    audit.save()
    resilience = {
        "schema_version": "memoryx.mx95.resilience-audit.v1",
        "run_id": args.run_id,
        "attempt": args.attempt,
        "observed_at_utc": datetime.now(UTC).isoformat(),
        "cases": cases,
        "limitations": [
            "The forced process death occurs after a committed response; it is not an in-flight persistence-boundary matrix.",
            "The migration fixture is deliberately damaged and is excluded from clean-base integrity claims until repair completes.",
            "These structural scenarios do not close N5 operation crash atomicity or prove hooks, compact/resume, cache reuse, or model quality.",
        ],
        "passed": all(item["passed"] for item in cases),
    }
    (output / "resilience-report.json").write_text(
        json.dumps(resilience, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )

    ledger_path = core_root / "coverage-ledger.json"
    ledger = json.loads(ledger_path.read_text(encoding="utf-8"))
    ledger["global_coverage_classes"] = list(dict.fromkeys(
        ledger["global_coverage_classes"]
        + ["crash_recovery", "migration", "deterministic_result"]
    ))
    ledger["global_evidence"] = [
        f"resilience-attempt-{args.attempt:02d}/resilience-report.json",
        f"resilience-attempt-{args.attempt:02d}/calls.jsonl",
        f"{args.core_attempt}/owner-contention.json",
        "surface.json",
    ]
    ledger["global_unresolved"] = [
        "No in-flight N5 persistence-boundary crash injection was performed; N5 remains open.",
        "Runtime behavior does not prove real hook/compact lifecycle, cache reuse, model quality, or total MemoryX semantic acceptance.",
    ]
    final_ledger = run_root / "coverage-ledger.final.json"
    final_ledger.write_text(json.dumps(ledger, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "passed": resilience["passed"], "cases": len(cases),
        "final_ledger": str(final_ledger),
    }, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
