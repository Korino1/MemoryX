"""Register source-backed MX-95 audit evidence in its durable module base."""

from __future__ import annotations

import argparse
import hashlib
import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from mcp_stdio import McpProcess
from run_full_audit import find_first, init_client, require_int, require_str, structured


def collect_values(value: Any, key: str) -> list[Any]:
    found: list[Any] = []
    if isinstance(value, dict):
        for name, child in value.items():
            if name == key:
                found.append(child)
            found.extend(collect_values(child, key))
    elif isinstance(value, list):
        for child in value:
            found.extend(collect_values(child, key))
    return found


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--repo-root", required=True, type=Path)
    parser.add_argument("--run-root", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--memoryx-version", required=True)
    parser.add_argument("--determinism-dir", default="determinism-postfix")
    parser.add_argument("--validator-controls-dir", default="validator-controls")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    repo = args.repo_root.resolve(strict=True)
    run_root = args.run_root.resolve(strict=True)
    output = args.output.resolve()
    if output.exists():
        raise FileExistsError(f"output exists: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)

    relative_sources = [
        "surface.json",
        "coverage-ledger.accepted.json",
        "postfix-run-summary.json",
        "resilience-attempt-01/resilience-report.json",
        f"{args.determinism_dir}/determinism-report.json",
        f"{args.validator_controls_dir}/mutation-control-report.json",
    ]
    source_specs = []
    for relative in relative_sources:
        path = run_root / relative
        content = path.read_bytes()
        source_specs.append({
            "relative_to_run": relative,
            "repo_relative": path.relative_to(repo).as_posix(),
            "sha256": hashlib.sha256(content).hexdigest(),
            "line_count": len(path.read_text(encoding="utf-8").splitlines()),
        })

    client = McpProcess(
        binary,
        repo,
        "mx-95-full-cross-testing",
        allow_existing=True,
        allow_durable_module_base=True,
    )
    exchanges: list[dict[str, Any]] = []

    def call(tool: str, arguments: dict[str, Any], request_id: str) -> dict[str, Any]:
        _, response = client.request(
            "tools/call", {"name": tool, "arguments": arguments}, request_id
        )
        exchanges.append({
            "request_id": request_id,
            "tool": tool,
            "arguments": arguments,
            "response": response,
        })
        if "error" in response or response.get("result", {}).get("isError") is True:
            raise RuntimeError(f"{tool} failed: {response}")
        return structured(response)

    try:
        init_client(client)
        registered = []
        for index, spec in enumerate(source_specs, start=1):
            value = call(
                "register_source",
                {
                    "kind": "file",
                    "label": f"MX-95 audit evidence: {spec['relative_to_run']}",
                    "path": spec["repo_relative"],
                    "line_start": 1,
                    "line_end": max(1, spec["line_count"]),
                    "source_version": f"sha256:{spec['sha256']}",
                },
                f"MX95-EVIDENCE-SOURCE-{index:02d}",
            )
            source_id = require_int(value, "source_id")
            registered.append({**spec, "source_id": source_id})

        atom_value = call(
            "ingest",
            {
                "atom_type": "FACT",
                "claims": [
                    {"subj": 95002006, "pred": 95, "obj_tag": 3, "obj_val": 47},
                    {"subj": 95002006, "pred": 96, "obj_tag": 3, "obj_val": 0},
                ],
                "symbols": [
                    f"mx95_run_{args.run_id}",
                    f"memoryx_{args.memoryx_version.replace('.', '_')}",
                    "observed_tools_47",
                    "direct_tools_47_of_47",
                    "relation_tombstone_resolution_cross_test_passed",
                ],
            },
            "MX95-EVIDENCE-ATOM",
        )
        atom_id = require_str(atom_value, "atom_id")
        for index, source in enumerate(registered, start=1):
            attached = call(
                "attach_atom_source",
                {"atom_id": atom_id, "source_id": source["source_id"]},
                f"MX95-EVIDENCE-ATTACH-{index:02d}",
            )
            if find_first(attached, "atom_id") != atom_id:
                raise RuntimeError("source attachment returned a different atom id")
        provenance = call(
            "get_provenance_path",
            {"atom_id": atom_id},
            "MX95-EVIDENCE-PROVENANCE",
        )
        provenance_source_ids = sorted(
            {
                value
                for value in collect_values(provenance, "source_id")
                if isinstance(value, int) and not isinstance(value, bool)
            }
        )
        registered_source_ids = sorted(item["source_id"] for item in registered)
        if not set(registered_source_ids).issubset(provenance_source_ids):
            raise RuntimeError(
                "provenance response omitted registered sources: "
                f"registered={registered_source_ids}, observed={provenance_source_ids}"
            )
        integrity = call(
            "verify_integrity", {}, "MX95-EVIDENCE-INTEGRITY"
        )
        if find_first(integrity, "valid") is not True:
            raise RuntimeError(f"durable module base integrity failed: {integrity}")
    finally:
        exit_code = client.close()
        client.write_logs(output.parent, output.stem)

    report = {
        "schema_version": "memoryx.mx95.evidence-registration.v1",
        "registered_at_utc": datetime.now(UTC).isoformat(),
        "base_path": ".memoryx/bases/mx-95-full-cross-testing",
        "atom_id": atom_id,
        "sources": registered,
        "provenance_source_ids": provenance_source_ids,
        "provenance_response": provenance,
        "integrity_valid": find_first(integrity, "valid"),
        "process_exit_code": exit_code,
        "foreign_process_action": "none",
        "exchanges": exchanges,
    }
    output.write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps({
        "atom_id": atom_id,
        "source_ids": [item["source_id"] for item in registered],
        "integrity_valid": report["integrity_valid"],
        "process_exit_code": exit_code,
    }, indent=2))
    return 0 if exit_code == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
