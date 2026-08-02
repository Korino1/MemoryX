"""Mutation controls for the MX-95 fail-closed coverage validator."""

from __future__ import annotations

import argparse
import copy
import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--validator", required=True, type=Path)
    parser.add_argument("--surface", required=True, type=Path)
    parser.add_argument("--ledger", required=True, type=Path)
    parser.add_argument("--calls", required=True, type=Path)
    parser.add_argument("--sequences", required=True, type=Path)
    parser.add_argument("--determinism", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    if output.exists():
        raise FileExistsError(f"output exists: {output}")
    output.mkdir(parents=True)

    surface = json.loads(args.surface.read_text(encoding="utf-8"))
    ledger = json.loads(args.ledger.read_text(encoding="utf-8"))
    calls = [json.loads(line) for line in args.calls.read_text(encoding="utf-8").splitlines() if line]
    sequences = json.loads(args.sequences.read_text(encoding="utf-8"))
    determinism = json.loads(args.determinism.read_text(encoding="utf-8"))
    validator_fixture = {
        "schema_version": "memoryx.mx95.validator-only-fixture.v1",
        "passed": True,
        "cases": [],
        "statement": "Synthetic only: isolates ledger validator controls and is not runtime acceptance evidence.",
    }
    resilience_path = output / "resilience-validator-only.json"
    write_json(resilience_path, validator_fixture)

    Mutation = Callable[
        [dict[str, Any], list[dict[str, Any]], dict[str, Any], dict[str, Any]],
        None,
    ]

    def missing_tool(l: dict[str, Any], _c: list[dict[str, Any]], _s: dict[str, Any], _d: dict[str, Any]) -> None:
        del l["tools"][0]

    def duplicate_tool(l: dict[str, Any], _c: list[dict[str, Any]], _s: dict[str, Any], _d: dict[str, Any]) -> None:
        l["tools"].append(copy.deepcopy(l["tools"][0]))

    def missing_direct(_l: dict[str, Any], c: list[dict[str, Any]], _s: dict[str, Any], _d: dict[str, Any]) -> None:
        direct_id = ledger["tools"][0]["direct_case_id"]
        c[:] = [item for item in c if item.get("case_id") != direct_id]

    def missing_cross(l: dict[str, Any], _c: list[dict[str, Any]], _s: dict[str, Any], _d: dict[str, Any]) -> None:
        l["tools"][0]["cross_sequence_ids"] = ["SQ-DOES-NOT-EXIST"]

    def absent_cross_evidence(
        _l: dict[str, Any], _c: list[dict[str, Any]], s: dict[str, Any], _d: dict[str, Any]
    ) -> None:
        s["sequences"][0]["observed_case_ids"].append("CASE-DOES-NOT-EXIST")

    def passed_without_evidence(
        _l: dict[str, Any], c: list[dict[str, Any]], _s: dict[str, Any], _d: dict[str, Any]
    ) -> None:
        direct_id = ledger["tools"][0]["direct_case_id"]
        next(item for item in c if item.get("case_id") == direct_id)["response_sha256"] = ""

    def nondeterministic_semantics(
        _l: dict[str, Any], _c: list[dict[str, Any]], _s: dict[str, Any], d: dict[str, Any]
    ) -> None:
        d["unique_all_semantic_results"] = 2

    def insufficient_determinism_repetitions(
        _l: dict[str, Any], _c: list[dict[str, Any]], _s: dict[str, Any], d: dict[str, Any]
    ) -> None:
        d["same_process_repetitions"] = 31

    mutations: list[tuple[str, Mutation | None, int]] = [
        ("baseline_join", None, 0),
        ("missing_tool", missing_tool, 1),
        ("duplicate_tool", duplicate_tool, 1),
        ("missing_direct_case", missing_direct, 1),
        ("missing_cross_case", missing_cross, 1),
        ("missing_cross_evidence", absent_cross_evidence, 1),
        ("passed_without_observed_evidence", passed_without_evidence, 1),
        ("nondeterministic_semantics", nondeterministic_semantics, 1),
        ("insufficient_determinism_repetitions", insufficient_determinism_repetitions, 1),
    ]
    results = []
    for name, mutation, expected in mutations:
        case_dir = output / name
        case_dir.mkdir()
        case_ledger = copy.deepcopy(ledger)
        case_calls = copy.deepcopy(calls)
        case_sequences = copy.deepcopy(sequences)
        case_determinism = copy.deepcopy(determinism)
        if mutation is not None:
            mutation(case_ledger, case_calls, case_sequences, case_determinism)
        ledger_path = case_dir / "ledger.json"
        calls_path = case_dir / "calls.jsonl"
        sequences_path = case_dir / "sequences.json"
        determinism_path = case_dir / "determinism.json"
        report_path = case_dir / "report.json"
        write_json(ledger_path, case_ledger)
        calls_path.write_text(
            "".join(json.dumps(item, ensure_ascii=False) + "\n" for item in case_calls),
            encoding="utf-8",
        )
        write_json(sequences_path, case_sequences)
        write_json(determinism_path, case_determinism)
        process = subprocess.run(
            [
                sys.executable,
                "-B",
                str(args.validator.resolve(strict=True)),
                "--surface", str(args.surface.resolve(strict=True)),
                "--ledger", str(ledger_path),
                "--calls", str(calls_path),
                "--sequences", str(sequences_path),
                "--resilience", str(resilience_path),
                "--determinism", str(determinism_path),
                "--report", str(report_path),
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        report = json.loads(report_path.read_text(encoding="utf-8"))
        results.append({
            "name": name,
            "expected_exit_code": expected,
            "observed_exit_code": process.returncode,
            "passed": process.returncode == expected,
            "validator_failures": report["failures"],
        })

    report = {
        "schema_version": "memoryx.mx95.validator-mutation-controls.v1",
        "synthetic_fixture_is_runtime_evidence": False,
        "results": results,
        "passed": all(item["passed"] for item in results),
    }
    write_json(output / "mutation-control-report.json", report)
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
