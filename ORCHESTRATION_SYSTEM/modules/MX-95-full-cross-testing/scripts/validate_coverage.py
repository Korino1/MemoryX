"""Fail-closed validator for the MX-95 observed all-tool coverage ledger."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--surface", required=True, type=Path)
    parser.add_argument("--ledger", required=True, type=Path)
    parser.add_argument("--calls", required=True, type=Path)
    parser.add_argument("--sequences", required=True, type=Path)
    parser.add_argument("--resilience", required=True, type=Path)
    parser.add_argument("--determinism", required=True, type=Path)
    parser.add_argument("--report", required=True, type=Path)
    args = parser.parse_args()

    surface = load_json(args.surface)
    ledger = load_json(args.ledger)
    calls = [json.loads(line) for line in args.calls.read_text(encoding="utf-8").splitlines() if line]
    sequences = load_json(args.sequences)
    resilience = load_json(args.resilience)
    determinism = load_json(args.determinism)
    failures: list[str] = []

    observed = surface.get("inventory", [])
    observed_names = [item.get("name") for item in observed]
    ledger_entries = ledger.get("tools", [])
    ledger_names = [item.get("name") for item in ledger_entries]
    call_by_case = {item.get("case_id"): item for item in calls if item.get("case_id")}
    sequence_by_id = {item.get("id"): item for item in sequences.get("sequences", [])}

    if len(observed_names) != len(set(observed_names)):
        failures.append("authoritative surface contains duplicate tool names")
    if len(ledger_names) != len(set(ledger_names)):
        failures.append("coverage ledger contains duplicate tool names")
    if set(observed_names) != set(ledger_names):
        failures.append(
            "ledger tool set differs from authoritative tools/list: "
            f"missing={sorted(set(observed_names)-set(ledger_names))}, "
            f"extra={sorted(set(ledger_names)-set(observed_names))}"
        )
    observed_digest = {item["name"]: item["schema_sha256"] for item in observed}

    for entry in ledger_entries:
        name = entry.get("name")
        if entry.get("schema_sha256") != observed_digest.get(name):
            failures.append(f"{name}: schema digest does not match authoritative surface")
        for required in (
            "purpose",
            "classification",
            "mcp_request_example",
            "cli_mapping",
            "rust_mapping",
            "direct_case_id",
            "cross_sequence_ids",
            "coverage_categories",
            "unresolved_limitations",
        ):
            if required not in entry or entry[required] in (None, "", []):
                failures.append(f"{name}: missing or empty {required}")
        direct_id = entry.get("direct_case_id")
        direct = call_by_case.get(direct_id)
        if direct is None:
            failures.append(f"{name}: direct case {direct_id!r} is absent")
        else:
            if direct.get("tool") != name:
                failures.append(f"{name}: direct case belongs to {direct.get('tool')!r}")
            if direct.get("status") != "passed":
                failures.append(f"{name}: direct case status is not passed")
            if not direct.get("request_sha256") or not direct.get("response_sha256"):
                failures.append(f"{name}: passed direct case lacks observed request/response evidence")
        for sequence_id in entry.get("cross_sequence_ids", []):
            sequence = sequence_by_id.get(sequence_id)
            if sequence is None:
                failures.append(f"{name}: cross sequence {sequence_id!r} is absent")
                continue
            if sequence.get("status") != "passed":
                failures.append(f"{name}: cross sequence {sequence_id} is not passed")
            if name not in sequence.get("tools", []):
                failures.append(f"{name}: cross sequence {sequence_id} does not contain the tool")
            if len(set(sequence.get("tools", []))) < 2:
                failures.append(f"{name}: cross sequence {sequence_id} is not cross-tool")
            evidence_cases = sequence.get("observed_case_ids", [])
            if not evidence_cases:
                failures.append(f"{name}: cross sequence {sequence_id} lacks observed cases")
            for case_id in evidence_cases:
                call = call_by_case.get(case_id)
                if call is None or not call.get("response_sha256"):
                    failures.append(
                        f"{name}: cross sequence {sequence_id} refers to missing evidence {case_id}"
                    )

    required_global = {
        "positive", "negative", "boundary", "stateful", "cross_call",
        "reopen_restart", "live_owner", "concurrency", "crash_recovery",
        "migration", "idempotence", "provenance", "conflict",
        "query_contract", "response_limit", "deterministic_result",
    }
    global_classes = set(ledger.get("global_coverage_classes", []))
    if not global_classes:
        failures.append("global coverage classes are absent")
    if missing_global := sorted(required_global - global_classes):
        failures.append(f"required global coverage classes are absent: {missing_global}")
    if resilience.get("passed") is not True:
        failures.append("resilience/migration/crash report did not pass")
    if determinism.get("passed") is not True:
        failures.append("post-fix deterministic query report did not pass")
    if determinism.get("same_process_repetitions", 0) < 32:
        failures.append("post-fix deterministic query report has fewer than 32 repetitions")
    if determinism.get("after_reopen_repetitions", 0) < 1:
        failures.append("post-fix deterministic query report lacks an after-reopen query")
    if determinism.get("unique_all_semantic_results") != 1:
        failures.append("post-fix semantic query result is not unique")
    if determinism.get("unique_incomplete_evidence_descriptions") != 1:
        failures.append("post-fix IncompleteEvidence description is not unique")
    if determinism.get("integrity_valid_after_reopen") is not True:
        failures.append("post-fix deterministic query base failed integrity after reopen")
    if determinism.get("process_exit_codes") != [0, 0]:
        failures.append("post-fix deterministic query owners did not exit cleanly")
    if not ledger.get("global_evidence"):
        failures.append("ledger lacks global evidence references")
    report = {
        "schema_version": "memoryx.mx95.coverage-validation.v1",
        "authoritative_tool_count": len(observed_names),
        "ledger_tool_count": len(ledger_names),
        "call_count": len(calls),
        "sequence_count": len(sequence_by_id),
        "resilience_case_count": len(resilience.get("cases", [])),
        "determinism_same_process_repetitions": determinism.get("same_process_repetitions"),
        "determinism_after_reopen_repetitions": determinism.get("after_reopen_repetitions"),
        "determinism_unique_semantic_results": determinism.get("unique_all_semantic_results"),
        "determinism_unique_descriptions": determinism.get("unique_incomplete_evidence_descriptions"),
        "failures": failures,
        "passed": not failures,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, indent=2))
    return 0 if not failures else 1


if __name__ == "__main__":
    raise SystemExit(main())
