"""Capture the authoritative first MemoryX tools/list for one MX-95 run."""

from __future__ import annotations

import argparse
import hashlib
import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from mcp_stdio import McpProcess, canonical_json


PROTOCOLS = ("2025-11-25", "2025-06-18", "2024-11-05")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--repo-root", required=True, type=Path)
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--expected-count", type=int, default=47)
    args = parser.parse_args()

    repo_root = args.repo_root.resolve(strict=True)
    output_root = args.output_root.resolve()
    module_root = (
        repo_root / "ORCHESTRATION_SYSTEM" / "modules" / "MX-95-full-cross-testing"
    ).resolve(strict=True)
    if module_root not in output_root.parents:
        raise ValueError("surface evidence output must stay inside MX-95")
    if output_root.exists():
        raise FileExistsError(f"run output already exists: {output_root}")
    output_root.mkdir(parents=True)
    raw_root = output_root / "raw"
    raw_root.mkdir()

    binary = args.binary.resolve(strict=True)
    binary_bytes = binary.read_bytes()
    identity = {
        "path": str(binary),
        "size_bytes": len(binary_bytes),
        "sha256": sha256_bytes(binary_bytes),
    }

    authoritative_tools: list[dict[str, Any]] | None = None
    authoritative_surface_digest: str | None = None
    lifecycle: list[dict[str, Any]] = []
    child_pids: list[int] = []

    for index, protocol in enumerate(PROTOCOLS, start=1):
        safe_protocol = protocol.replace("-", "")
        base_name = f"mx-95-disposable-{args.run_id}-surface-{index}-{safe_protocol}"
        client = McpProcess(binary, repo_root, base_name)
        child_pids.append(client.process.pid)
        stem = f"protocol-{index}-{protocol}"
        try:
            initialize_line, initialize = client.request(
                "initialize",
                {
                    "protocolVersion": protocol,
                    "capabilities": {},
                    "clientInfo": {"name": "mx-95-audit", "version": "1"},
                },
                request_id=f"initialize-{protocol}",
            )
            if initialize.get("result", {}).get("protocolVersion") != protocol:
                raise RuntimeError(f"protocol {protocol} was not echoed exactly")
            client.notify("notifications/initialized")
            client.assert_notification_silence()
            tools_line, tools_response = client.request(
                "tools/list", {}, request_id=f"tools-list-{protocol}"
            )
            tools = tools_response.get("result", {}).get("tools")
            if not isinstance(tools, list):
                raise RuntimeError("tools/list did not return a tools array")
            names = [tool.get("name") for tool in tools]
            duplicates = sorted({name for name in names if names.count(name) > 1})
            surface_digest = sha256_bytes(canonical_json(tools))
            if authoritative_tools is None:
                authoritative_tools = tools
                authoritative_surface_digest = surface_digest
                (raw_root / "authoritative-initialize.response.jsonl").write_text(
                    initialize_line + "\n", encoding="utf-8"
                )
                (raw_root / "authoritative-tools-list.response.jsonl").write_text(
                    tools_line + "\n", encoding="utf-8"
                )
            elif surface_digest != authoritative_surface_digest:
                raise RuntimeError(
                    f"tool surface changed for protocol {protocol}: {surface_digest}"
                )
            lifecycle.append(
                {
                    "protocol": protocol,
                    "initialize_ok": True,
                    "notification_silent": True,
                    "tools_count": len(tools),
                    "unique_count": len(set(names)),
                    "duplicates": duplicates,
                    "surface_digest": surface_digest,
                    "base_name": base_name,
                    "base_path": str(client.base_path),
                    "child_pid": client.process.pid,
                }
            )
        finally:
            exit_code = client.close()
            client.write_logs(raw_root, stem)
            lifecycle[-1]["exit_code"] = exit_code if lifecycle else exit_code
            lifecycle[-1]["orphan_after_close"] = client.process.poll() is None

    assert authoritative_tools is not None
    names = [tool["name"] for tool in authoritative_tools]
    inventory = []
    for tool in authoritative_tools:
        schema = tool.get("inputSchema")
        inventory.append(
            {
                "name": tool["name"],
                "description": tool.get("description", ""),
                "input_schema": schema,
                "schema_sha256": sha256_bytes(canonical_json(schema)),
                "published_examples": (schema or {}).get("examples", []),
            }
        )
    inventory.sort(key=lambda item: item["name"])
    duplicate_names = sorted({name for name in names if names.count(name) > 1})
    count_matches_expectation = len(authoritative_tools) == args.expected_count
    unique_matches_count = len(set(names)) == len(authoritative_tools)
    report = {
        "schema_version": "memoryx.mx95.surface-capture.v1",
        "run_id": args.run_id,
        "observed_at_utc": datetime.now(UTC).isoformat(),
        "binary": identity,
        "authoritative_protocol": PROTOCOLS[0],
        "supported_protocols_tested": list(PROTOCOLS),
        "authoritative_surface_sha256": authoritative_surface_digest,
        "observed_count": len(authoritative_tools),
        "observed_unique_count": len(set(names)),
        "expected_count_assertion": args.expected_count,
        "count_matches_expectation": count_matches_expectation,
        "unique_matches_count": unique_matches_count,
        "duplicate_names": duplicate_names,
        "lifecycle": lifecycle,
        "child_pids": child_pids,
        "inventory": inventory,
        "gate_passed": (
            count_matches_expectation
            and unique_matches_count
            and not duplicate_names
            and all(
                item["initialize_ok"]
                and item["notification_silent"]
                and item["exit_code"] == 0
                and not item["orphan_after_close"]
                for item in lifecycle
            )
        ),
    }
    (output_root / "surface.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    (output_root / "inventory.seed.json").write_text(
        json.dumps(inventory, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps({key: report[key] for key in (
        "run_id",
        "observed_count",
        "observed_unique_count",
        "expected_count_assertion",
        "count_matches_expectation",
        "duplicate_names",
        "authoritative_surface_sha256",
        "gate_passed",
    )}, indent=2))
    return 0 if report["gate_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
