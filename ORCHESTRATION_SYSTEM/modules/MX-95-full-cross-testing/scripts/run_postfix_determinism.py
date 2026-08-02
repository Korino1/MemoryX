"""Post-fix deterministic query gate for MX95-001."""

from __future__ import annotations

import argparse
import hashlib
import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from mcp_stdio import McpProcess, canonical_json
from run_full_audit import Audit, find_first, init_client


def digest(value: Any) -> str:
    return hashlib.sha256(canonical_json(value)).hexdigest()


def incomplete_evidence_description(answer: Any) -> str | None:
    if not isinstance(answer, dict):
        return None
    limitations = answer.get("limitations")
    if not isinstance(limitations, list):
        return None
    descriptions = [
        item.get("description")
        for item in limitations
        if isinstance(item, dict)
        and item.get("code") == "IncompleteEvidence"
        and isinstance(item.get("description"), str)
    ]
    return descriptions[0] if len(descriptions) == 1 else None


def semantic_projection(answer: Any) -> Any:
    """Remove framing telemetry while retaining semantic AnswerPack content."""
    if not isinstance(answer, dict):
        return answer
    projected = json.loads(json.dumps(answer, ensure_ascii=False))
    response_limits = projected.get("response_limits")
    if isinstance(response_limits, dict):
        response_limits.pop("emitted_bytes", None)
        response_limits.pop("original_bytes", None)
    return projected


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--repo-root", required=True, type=Path)
    parser.add_argument("--base-name", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--repetitions", required=True, type=int)
    args = parser.parse_args()
    if args.repetitions < 32 or args.repetitions > 256:
        raise ValueError("repetitions must be between 32 and 256")
    binary = args.binary.resolve(strict=True)
    repo = args.repo_root.resolve(strict=True)
    output = args.output.resolve()
    if output.exists():
        raise FileExistsError(f"output exists: {output}")
    output.mkdir(parents=True)
    audit = Audit(output)
    answers: list[dict[str, Any]] = []
    process_exits: list[int] = []

    first = McpProcess(binary, repo, args.base_name, allow_existing=True)
    try:
        init_client(first)
        for index in range(args.repetitions):
            answer = audit.call(
                first,
                "query",
                {"query_text": "mx95_provenance_marker", "ctx_id": 0},
                f"PF-DET-{index + 1:03d}",
                "SQ-POSTFIX-DETERMINISM",
                direct=False,
            )
            answers.append({"phase": "same_process", "iteration": index + 1, "answer": answer})
    finally:
        first_exit = first.close()
        process_exits.append(first_exit)
        first.write_logs(output, "same-process")

    reopened = McpProcess(binary, repo, args.base_name, allow_existing=True)
    try:
        init_client(reopened)
        answer = audit.call(
            reopened,
            "query",
            {"query_text": "mx95_provenance_marker", "ctx_id": 0},
            "PF-DET-REOPEN-001",
            "SQ-POSTFIX-DETERMINISM",
            direct=False,
        )
        answers.append({"phase": "after_reopen", "iteration": 1, "answer": answer})
        integrity = audit.call(
            reopened,
            "verify_integrity",
            {},
            "PF-DET-REOPEN-INTEGRITY",
            "SQ-POSTFIX-DETERMINISM",
            direct=False,
        )
    finally:
        reopened_exit = reopened.close()
        process_exits.append(reopened_exit)
        reopened.write_logs(output, "after-reopen")

    audit.save()
    observations = []
    for item in answers:
        answer = item["answer"]
        observations.append({
            "phase": item["phase"],
            "iteration": item["iteration"],
            "full_answer_sha256": digest(answer),
            "semantic_projection_sha256": digest(semantic_projection(answer)),
            "incomplete_evidence_description": incomplete_evidence_description(answer),
            "status": find_first(answer, "status"),
            "snapshot": find_first(answer, "snapshot"),
        })

    same_process = [item for item in observations if item["phase"] == "same_process"]
    unique_same_full = sorted({item["full_answer_sha256"] for item in same_process})
    unique_same_semantic = sorted(
        {item["semantic_projection_sha256"] for item in same_process}
    )
    unique_all_full = sorted({item["full_answer_sha256"] for item in observations})
    unique_all_semantic = sorted(
        {item["semantic_projection_sha256"] for item in observations}
    )
    descriptions = [item["incomplete_evidence_description"] for item in observations]
    unique_descriptions = sorted({value for value in descriptions if value is not None})
    passed = (
        len(same_process) == args.repetitions
        and len(observations) == args.repetitions + 1
        and len(unique_same_semantic) == 1
        and len(unique_all_semantic) == 1
        and len(unique_descriptions) == 1
        and all(value is not None for value in descriptions)
        and find_first(integrity, "valid") is True
        and process_exits == [0, 0]
    )
    report = {
        "schema_version": "memoryx.mx95.postfix-determinism.v1",
        "observed_at_utc": datetime.now(UTC).isoformat(),
        "binary": {
            "path": str(binary),
            "size_bytes": binary.stat().st_size,
            "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        },
        "base_name": args.base_name,
        "query_arguments": {"query_text": "mx95_provenance_marker", "ctx_id": 0},
        "same_process_repetitions": args.repetitions,
        "after_reopen_repetitions": 1,
        "observations": observations,
        "unique_same_process_full_results": len(unique_same_full),
        "unique_same_process_semantic_results": len(unique_same_semantic),
        "unique_all_full_results": len(unique_all_full),
        "unique_all_semantic_results": len(unique_all_semantic),
        "unique_incomplete_evidence_descriptions": len(unique_descriptions),
        "stable_incomplete_evidence_description": (
            unique_descriptions[0] if len(unique_descriptions) == 1 else None
        ),
        "semantic_projection_excluded_fields": [
            "response_limits.emitted_bytes",
            "response_limits.original_bytes"
        ],
        "integrity_valid_after_reopen": find_first(integrity, "valid"),
        "process_exit_codes": process_exits,
        "passed": passed,
        "limitations": [
            "Full serialized AnswerPacks may differ in response framing byte counters; those counters are reported separately and are excluded from the semantic projection.",
            "This gate covers one module-owned snapshot and one query shape; it is not total MemoryX semantic acceptance.",
            "It does not prove hook lifecycle, compact/resume, cache reuse, model quality, or N5 completion.",
        ],
    }
    (output / "determinism-report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps({
        "passed": passed,
        "same_process_repetitions": args.repetitions,
        "after_reopen_repetitions": 1,
        "unique_all_full_results": len(unique_all_full),
        "unique_all_semantic_results": len(unique_all_semantic),
        "unique_descriptions": len(unique_descriptions),
        "process_exit_codes": process_exits,
    }, indent=2))
    return 0 if passed else 2


if __name__ == "__main__":
    raise SystemExit(main())
