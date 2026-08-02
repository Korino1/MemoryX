"""Reproduce MX95-001 without mutating the module-owned disposable base."""

from __future__ import annotations

import argparse
import hashlib
import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from mcp_stdio import McpProcess
from run_full_audit import init_client


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--repo-root", required=True, type=Path)
    parser.add_argument("--base-name", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--repetitions", type=int, default=8)
    args = parser.parse_args()
    if args.repetitions < 2 or args.repetitions > 32:
        raise ValueError("repetitions must be between 2 and 32")
    output = args.output.resolve()
    if output.exists():
        raise FileExistsError(f"output exists: {output}")
    output.mkdir(parents=True)

    client = McpProcess(
        args.binary.resolve(strict=True),
        args.repo_root.resolve(strict=True),
        args.base_name,
        allow_existing=True,
    )
    observations: list[dict[str, Any]] = []
    try:
        init_client(client)
        for index in range(args.repetitions):
            _, response = client.request(
                "tools/call",
                {
                    "name": "query",
                    "arguments": {
                        "query_text": "mx95_provenance_marker",
                        "ctx_id": 0,
                    },
                },
                f"MX95-001-{index + 1:02d}",
            )
            result_text = response["result"]["structuredContent"]["text"]
            answer = json.loads(result_text)
            incomplete = next(
                item["description"]
                for item in answer["limitations"]
                if item["code"] == "IncompleteEvidence"
            )
            observations.append(
                {
                    "iteration": index + 1,
                    "result_sha256": hashlib.sha256(result_text.encode("utf-8")).hexdigest(),
                    "incomplete_evidence_description": incomplete,
                    "status": answer["status"],
                    "snapshot": answer["snapshot"],
                }
            )
    finally:
        exit_code = client.close()
        client.write_logs(output, "reproduction")

    distinct_results = sorted({item["result_sha256"] for item in observations})
    distinct_descriptions = sorted(
        {item["incomplete_evidence_description"] for item in observations}
    )
    confirmed = len(distinct_results) > 1 and len(distinct_descriptions) > 1
    report = {
        "schema_version": "memoryx.mx95.runtime-defect-reproduction.v1",
        "defect_id": "MX95-001",
        "observed_at_utc": datetime.now(UTC).isoformat(),
        "binary": str(args.binary.resolve(strict=True)),
        "base_name": args.base_name,
        "request": {
            "tool": "query",
            "arguments": {"query_text": "mx95_provenance_marker", "ctx_id": 0},
        },
        "repetitions": args.repetitions,
        "observations": observations,
        "distinct_result_count": len(distinct_results),
        "distinct_description_count": len(distinct_descriptions),
        "clean_exit": exit_code == 0,
        "confirmed": confirmed,
        "inference": (
            "The returned AnswerPack changes the ordering of the uncovered-gap set "
            "inside the user-visible IncompleteEvidence description."
        ),
        "boundary": (
            "This report demonstrates response nondeterminism only; it does not identify "
            "a production source fix and does not close N5."
        ),
    }
    (output / "reproduction-report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps({
        "confirmed": confirmed,
        "distinct_results": len(distinct_results),
        "distinct_descriptions": len(distinct_descriptions),
        "clean_exit": exit_code == 0,
    }, indent=2))
    return 2 if confirmed else 1


if __name__ == "__main__":
    raise SystemExit(main())
