"""Join post-fix MX-95 evidence into an acceptance ledger and run summary."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


def load(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--run-root", required=True, type=Path)
    parser.add_argument("--core-attempt", required=True)
    parser.add_argument("--resilience-attempt", required=True)
    parser.add_argument("--determinism-dir", required=True)
    parser.add_argument("--validator-controls", required=True)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--expected-sha256", required=True)
    parser.add_argument("--expected-size", required=True, type=int)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = args.run_root.resolve(strict=True)
    core = root / args.core_attempt
    resilience_dir = root / args.resilience_attempt
    determinism_dir = root / args.determinism_dir
    validator_controls_path = root / args.validator_controls / "mutation-control-report.json"
    surface = load(root / "surface.json")
    core_summary = load(core / "run-summary.json")
    ledger = load(root / "coverage-ledger.final.json")
    resilience = load(resilience_dir / "resilience-report.json")
    determinism = load(determinism_dir / "determinism-report.json")
    validator_controls = load(validator_controls_path)

    binary_bytes = binary.read_bytes()
    observed_sha256 = hashlib.sha256(binary_bytes).hexdigest()
    observed_version = subprocess.check_output(
        [str(binary), "--version"], text=True, encoding="utf-8"
    ).strip()
    binary_identity_passed = (
        observed_version == f"memoryx {args.expected_version}"
        and len(binary_bytes) == args.expected_size
        and observed_sha256.lower() == args.expected_sha256.lower()
        and surface["binary"]["sha256"].lower() == observed_sha256.lower()
        and surface["binary"]["size_bytes"] == len(binary_bytes)
    )
    determinism_passed = (
        determinism.get("passed") is True
        and determinism.get("same_process_repetitions", 0) >= 32
        and determinism.get("after_reopen_repetitions", 0) >= 1
        and determinism.get("unique_all_semantic_results") == 1
        and determinism.get("unique_incomplete_evidence_descriptions") == 1
        and determinism.get("integrity_valid_after_reopen") is True
        and determinism.get("process_exit_codes") == [0, 0]
    )
    core_passed = (
        core_summary.get("direct_tools_passed") == surface.get("observed_count")
        and core_summary.get("authoritative_tools") == surface.get("observed_count")
        and core_summary.get("sequences", 0) > 0
        and core_summary.get("primary_exit") == 0
        and core_summary.get("reopen_exit") == 0
    )
    passed = (
        binary_identity_passed
        and surface.get("gate_passed") is True
        and core_passed
        and resilience.get("passed") is True
        and determinism_passed
        and validator_controls.get("passed") is True
    )

    ledger["global_evidence"] = list(dict.fromkeys(
        ledger.get("global_evidence", [])
        + [
            f"{args.determinism_dir}/determinism-report.json",
            f"{args.determinism_dir}/calls.jsonl",
            f"{args.validator_controls}/mutation-control-report.json",
        ]
    ))
    ledger["binary_identity"] = {
        "version": observed_version,
        "size_bytes": len(binary_bytes),
        "sha256": observed_sha256,
        "passed": binary_identity_passed,
    }
    ledger["postfix_determinism_gate"] = {
        "same_process_repetitions": determinism["same_process_repetitions"],
        "after_reopen_repetitions": determinism["after_reopen_repetitions"],
        "unique_full_results": determinism["unique_all_full_results"],
        "unique_semantic_results": determinism["unique_all_semantic_results"],
        "unique_incomplete_evidence_descriptions": determinism[
            "unique_incomplete_evidence_descriptions"
        ],
        "stable_incomplete_evidence_description": determinism[
            "stable_incomplete_evidence_description"
        ],
        "semantic_projection_excluded_fields": determinism[
            "semantic_projection_excluded_fields"
        ],
        "passed": determinism_passed,
    }
    ledger["global_gate_status"] = "passed" if passed else "failed"
    ledger["global_unresolved"] = [
        "Full AnswerPack response framing byte counters varied while semantic content and IncompleteEvidence description remained stable.",
        "No in-flight N5 persistence-boundary crash injection was performed; N5 remains open.",
        "Runtime behavior does not prove real hook/compact lifecycle, cache reuse, model quality, or total MemoryX semantic acceptance.",
    ]
    accepted_ledger = root / "coverage-ledger.accepted.json"
    accepted_ledger.write_text(
        json.dumps(ledger, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )

    report = {
        "schema_version": "memoryx.mx95.postfix-run-summary.v1",
        "run_id": root.name,
        "observed_at_utc": datetime.now(UTC).isoformat(),
        "binary_identity": ledger["binary_identity"],
        "surface": {
            "observed_count": surface["observed_count"],
            "observed_unique_count": surface["observed_unique_count"],
            "surface_sha256": surface["authoritative_surface_sha256"],
            "supported_protocols_tested": surface["supported_protocols_tested"],
            "passed": surface["gate_passed"],
        },
        "core": core_summary,
        "resilience_passed": resilience["passed"],
        "resilience_case_count": len(resilience["cases"]),
        "postfix_determinism": ledger["postfix_determinism_gate"],
        "validator_controls_passed": validator_controls.get("passed"),
        "validator_control_count": len(validator_controls.get("results", [])),
        "passed": passed,
        "limitations": ledger["global_unresolved"],
    }
    (root / "postfix-run-summary.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
