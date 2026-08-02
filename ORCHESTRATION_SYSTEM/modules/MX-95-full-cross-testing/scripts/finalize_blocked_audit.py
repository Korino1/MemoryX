"""Assemble the MX-95 blocked audit from preserved observed evidence."""

from __future__ import annotations

import argparse
import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


def jsonl(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line]


def by_id(rows: list[dict[str, Any]], request_id: str) -> dict[str, Any]:
    return next(row for row in rows if row.get("id") == request_id)


def structured(row: dict[str, Any]) -> dict[str, Any]:
    value = row["result"]["structuredContent"]
    if value.get("schema_version") == "memoryx.text-result.v1":
        try:
            return json.loads(value["text"])
        except json.JSONDecodeError:
            return value
    return value


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--run-root", required=True, type=Path)
    args = parser.parse_args()
    root = args.run_root.resolve(strict=True)
    core = root / "full-attempt-03"
    attempt = root / "resilience-attempt-01"
    defect_path = root / "defects" / "MX95-001" / "reproduction-report.json"

    limits = structured(by_id(jsonl(attempt / "determinism-first.stdout.jsonl"), "E-LIMIT-002-4"))
    limit_report = limits["response_limits"]
    migration_rows = jsonl(attempt / "migration-apply.stdout.jsonl")
    migration_before = structured(by_id(migration_rows, "E-MIG-005-11"))["audit"]
    migration_dry = structured(by_id(migration_rows, "E-MIG-006-12"))
    migration_apply = structured(by_id(migration_rows, "E-MIG-007-13"))
    migration_after = structured(by_id(migration_rows, "E-MIG-008-14"))["audit"]
    migration_repeat = structured(by_id(migration_rows, "E-MIG-009-15"))
    crash_rows = jsonl(attempt / "crash-reopen.stdout.jsonl")
    crash_lookup = structured(by_id(crash_rows, "E-CRASH-001-16"))
    crash_integrity = structured(by_id(crash_rows, "E-CRASH-002-17"))
    defect = json.loads(defect_path.read_text(encoding="utf-8"))

    response_limit_passed = (
        limit_report["max_bytes"] == 2048
        and limit_report["max_items"] == 1
        and limit_report["emitted_bytes"] <= 2048
        and limit_report["bytes_truncated"] is True
        and limit_report["items_truncated"] is True
    )
    migration_passed = (
        migration_before["consistent"] is False
        and migration_dry["eligible_relation_ids"] == [1]
        and migration_apply["mutated"] is True
        and migration_after["consistent"] is True
        and migration_repeat["mutated"] is False
    )
    crash_passed = (
        "count=1" in crash_lookup["text"]
        and crash_integrity["valid"] is True
        and crash_integrity["summary"]["checked_atoms"] == 1
    )
    deterministic_passed = defect["confirmed"] is False

    cases = [
        {
            "name": "query_response_limit_enforced",
            "status": "passed" if response_limit_passed else "failed",
            "evidence": limit_report,
            "source": "resilience-attempt-01/determinism-first.stdout.jsonl",
        },
        {
            "name": "relation_context_migration_and_idempotence",
            "status": "passed" if migration_passed else "failed",
            "evidence": {
                "before_consistent": migration_before["consistent"],
                "eligible_relation_ids": migration_dry["eligible_relation_ids"],
                "apply_mutated": migration_apply["mutated"],
                "after_consistent": migration_after["consistent"],
                "repeat_mutated": migration_repeat["mutated"],
            },
            "source": "resilience-attempt-01/migration-apply.stdout.jsonl",
        },
        {
            "name": "post_commit_process_death_recovery",
            "status": "passed" if crash_passed else "failed",
            "evidence": {
                "lookup_text": crash_lookup["text"],
                "integrity_valid": crash_integrity["valid"],
                "checked_atoms": crash_integrity["summary"]["checked_atoms"],
            },
            "source": "resilience-attempt-01/crash-reopen.stdout.jsonl",
        },
        {
            "name": "identical_query_result_determinism",
            "status": "passed" if deterministic_passed else "failed",
            "evidence": {
                "repetitions": defect["repetitions"],
                "distinct_result_count": defect["distinct_result_count"],
                "distinct_description_count": defect["distinct_description_count"],
                "defect_id": defect["defect_id"],
            },
            "source": "defects/MX95-001/reproduction-report.json",
        },
    ]
    resilience = {
        "schema_version": "memoryx.mx95.resilience-audit.v1",
        "run_id": root.name,
        "observed_at_utc": datetime.now(UTC).isoformat(),
        "cases": cases,
        "blocked_by": ["MX95-001: identical query responses are nondeterministic"],
        "limitations": [
            "Process death was injected only after a committed response, not at an N5 persistence boundary.",
            "The migration base was deliberately damaged and was repaired before its final consistent result.",
            "No structural result proves hooks, compact/resume, cache reuse, model quality, total semantic acceptance, or N5 completion.",
        ],
        "passed": False,
    }
    resilience_path = root / "resilience-blocked-report.json"
    resilience_path.write_text(
        json.dumps(resilience, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )

    ledger = json.loads((core / "coverage-ledger.json").read_text(encoding="utf-8"))
    ledger["global_coverage_classes"] = list(
        dict.fromkeys(
            ledger["global_coverage_classes"]
            + ["crash_recovery", "migration", "deterministic_result"]
        )
    )
    ledger["global_evidence"] = [
        "resilience-blocked-report.json",
        "resilience-attempt-01/determinism-first.stdout.jsonl",
        "resilience-attempt-01/migration-apply.stdout.jsonl",
        "resilience-attempt-01/crash-reopen.stdout.jsonl",
        "defects/MX95-001/reproduction-report.json",
        "full-attempt-03/owner-contention.json",
        "surface.json",
    ]
    ledger["global_gate_status"] = "blocked"
    ledger["blocked_by"] = resilience["blocked_by"]
    ledger["global_unresolved"] = [
        "MX95-001 blocks deterministic result acceptance.",
        "No in-flight N5 persistence-boundary crash injection was performed; N5 remains open.",
        "Runtime behavior does not prove hook/compact lifecycle, cache reuse, model quality, or total MemoryX semantic acceptance.",
    ]
    ledger_path = root / "coverage-ledger.blocked.json"
    ledger_path.write_text(
        json.dumps(ledger, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps({
        "resilience_passed": resilience["passed"],
        "passed_cases": sum(item["status"] == "passed" for item in cases),
        "failed_cases": sum(item["status"] == "failed" for item in cases),
        "blocked_by": resilience["blocked_by"],
        "ledger": str(ledger_path),
    }, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
