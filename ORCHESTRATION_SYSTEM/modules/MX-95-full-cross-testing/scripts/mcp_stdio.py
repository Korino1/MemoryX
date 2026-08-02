"""Bounded JSONL MCP stdio client used only by the MX-95 audit harness."""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time
from pathlib import Path
from typing import Any


class McpProtocolError(RuntimeError):
    """Raised when the child violates the expected JSON-RPC lifecycle."""


class McpProcess:
    def __init__(
        self,
        binary: Path,
        repo_root: Path,
        base_name: str,
        timeout_seconds: float = 15.0,
        allow_existing: bool = False,
        allow_durable_module_base: bool = False,
    ) -> None:
        if not base_name.startswith("mx-95-disposable-") and not (
            allow_durable_module_base and base_name == "mx-95-full-cross-testing"
        ):
            raise ValueError("MX-95 runtime bases must use the disposable prefix")
        self.binary = binary.resolve(strict=True)
        self.repo_root = repo_root.resolve(strict=True)
        self.base_name = base_name
        self.base_path = (self.repo_root / ".memoryx" / "bases" / base_name).resolve()
        allowed_root = (self.repo_root / ".memoryx" / "bases").resolve()
        if self.base_path.parent != allowed_root:
            raise ValueError("runtime base escaped the project-local bases directory")
        if self.base_path.exists() and not allow_existing:
            raise FileExistsError(f"refusing to reuse runtime base: {self.base_path}")
        self.timeout_seconds = timeout_seconds
        self.stdout_lines: list[str] = []
        self.stderr_lines: list[str] = []
        self._stdout_queue: queue.Queue[str | None] = queue.Queue()
        self._next_id = 1
        creation_flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
        self.process = subprocess.Popen(
            [
                str(self.binary),
                "--base-scope",
                "project",
                "serve",
                "--base",
                base_name,
                "--stdio",
            ],
            cwd=self.repo_root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="strict",
            bufsize=1,
            creationflags=creation_flags,
        )
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        self._stdout_thread = threading.Thread(
            target=self._read_stdout, name=f"mx95-stdout-{self.process.pid}", daemon=True
        )
        self._stderr_thread = threading.Thread(
            target=self._read_stderr, name=f"mx95-stderr-{self.process.pid}", daemon=True
        )
        self._stdout_thread.start()
        self._stderr_thread.start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        try:
            for line in self.process.stdout:
                stripped = line.rstrip("\r\n")
                self.stdout_lines.append(stripped)
                self._stdout_queue.put(stripped)
        finally:
            self._stdout_queue.put(None)

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        for line in self.process.stderr:
            self.stderr_lines.append(line.rstrip("\r\n"))

    def send(self, payload: dict[str, Any]) -> None:
        if self.process.poll() is not None:
            raise McpProtocolError(
                f"MCP child exited before send with code {self.process.returncode}"
            )
        assert self.process.stdin is not None
        line = json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
        self.process.stdin.write(line + "\n")
        self.process.stdin.flush()

    def read_response(self, timeout_seconds: float | None = None) -> tuple[str, dict[str, Any]]:
        timeout = self.timeout_seconds if timeout_seconds is None else timeout_seconds
        try:
            line = self._stdout_queue.get(timeout=timeout)
        except queue.Empty as error:
            raise TimeoutError(f"no MCP response within {timeout:.3f}s") from error
        if line is None:
            raise McpProtocolError(
                f"MCP stdout closed with exit code {self.process.poll()}"
            )
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise McpProtocolError(f"non-JSON stdout line: {line!r}") from error
        if not isinstance(value, dict):
            raise McpProtocolError("MCP response is not a JSON object")
        return line, value

    def request(
        self, method: str, params: dict[str, Any] | None = None, request_id: Any | None = None
    ) -> tuple[str, dict[str, Any]]:
        if request_id is None:
            request_id = f"mx95-{self._next_id}"
            self._next_id += 1
        payload: dict[str, Any] = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
        }
        if params is not None:
            payload["params"] = params
        self.send(payload)
        line, response = self.read_response()
        if response.get("id") != request_id:
            raise McpProtocolError(
                f"response id {response.get('id')!r} did not match {request_id!r}"
            )
        return line, response

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        payload: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            payload["params"] = params
        self.send(payload)

    def assert_notification_silence(self, window_seconds: float = 0.35) -> None:
        time.sleep(window_seconds)
        try:
            line = self._stdout_queue.get_nowait()
        except queue.Empty:
            return
        if line is None:
            raise McpProtocolError("MCP stdout closed after notification")
        raise McpProtocolError(f"notification produced a response: {line}")

    def close(self, timeout_seconds: float = 10.0) -> int:
        if self.process.stdin is not None and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            return self.process.wait(timeout=timeout_seconds)
        except subprocess.TimeoutExpired:
            # This is our own verified child. Termination is bounded and never targets
            # a foreign PID.
            self.process.terminate()
            try:
                return self.process.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
                self.process.kill()
                return self.process.wait(timeout=3.0)

    def write_logs(self, directory: Path, stem: str) -> None:
        directory.mkdir(parents=True, exist_ok=True)
        (directory / f"{stem}.stdout.jsonl").write_text(
            "\n".join(self.stdout_lines) + ("\n" if self.stdout_lines else ""),
            encoding="utf-8",
        )
        (directory / f"{stem}.stderr.log").write_text(
            "\n".join(self.stderr_lines) + ("\n" if self.stderr_lines else ""),
            encoding="utf-8",
        )


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
