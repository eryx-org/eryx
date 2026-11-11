"""Result and limit types, shaped like pyeryx's."""

from __future__ import annotations

import abc
import json
from dataclasses import dataclass, field
from typing import Any, Optional


@dataclass(frozen=True)
class ExecuteResult(abc.ABC):  # noqa: B024 - an ABC so pyeryx's class can register with it
    """Result of executing Python code in the sandbox.

    The same fields as pyeryx's ``ExecuteResult``, plus a few only the hosted
    API has (``network_bytes``, ``trace_id``, ``unsaved_variables``).
    """

    stdout: bytes = b""
    """Complete stdout output from the sandboxed code as raw bytes."""

    stderr: bytes = b""
    """Complete stderr output from the sandboxed code as raw bytes."""

    duration_ms: float = 0.0
    """Execution duration in milliseconds."""

    callback_invocations: int = 0
    """Number of callback invocations during execution."""

    peak_memory_bytes: Optional[int] = None
    """Peak memory usage in bytes (if available)."""

    fuel_consumed: Optional[int] = None
    """Fuel (WASM instructions) consumed: eryx.run's compute units."""

    result_json: Optional[str] = None
    """The raw JSON of the script's ``result`` variable, or ``None``."""

    result_error: Optional[str] = None
    """Why ``result`` couldn't be captured (e.g. not JSON-serializable)."""

    state_was_reset: bool = False
    """Whether a session lost its state before this run. Hosted sessions never
    do: each run restores the saved state into a fresh sandbox."""

    network_bytes: Optional[int] = None
    """Bytes the run sent and received over the network (hosted only)."""

    trace_id: Optional[str] = None
    """The run's trace, when it was traced (hosted only, over the stream)."""

    unsaved_variables: list[str] = field(default_factory=list)
    """Session variables that couldn't be saved (hosted only)."""

    @property
    def stdout_text(self) -> str:
        """Stdout decoded as UTF-8 with replacement characters for invalid bytes."""
        return self.stdout.decode("utf-8", errors="replace")

    @property
    def stderr_text(self) -> str:
        """Stderr decoded as UTF-8 with replacement characters for invalid bytes."""
        return self.stderr.decode("utf-8", errors="replace")

    @property
    def result(self) -> Any:
        """The script's ``result`` variable, parsed from JSON, or ``None`` if it
        was not set."""
        return None if self.result_json is None else json.loads(self.result_json)


@dataclass(frozen=True)
class ResourceLimits:
    """Limits for a run, with the same fields as pyeryx's ``ResourceLimits``.

    ``None`` means the backend's default: on eryx.run, your org's limit. A
    value over your org's limit is capped at it. The hosted API takes
    ``execution_timeout_ms`` (rounded up to whole seconds) and ``max_fuel``;
    setting the others raises ``ValueError`` when you run code, since the
    service uses its own.
    """

    execution_timeout_ms: Optional[int] = None
    callback_timeout_ms: Optional[int] = None
    max_memory_bytes: Optional[int] = None
    max_callback_invocations: Optional[int] = None
    max_fuel: Optional[int] = None
    max_vfs_bytes: Optional[int] = None
    max_open_host_files: Optional[int] = None

    @staticmethod
    def unlimited() -> ResourceLimits:
        """No limits of your own: on eryx.run, your org's."""
        return ResourceLimits()

    def hosted_request(self) -> dict[str, int]:
        """The request fields for these limits, or ``ValueError`` for ones the
        hosted API can't take."""
        unsupported = [
            name
            for name in (
                "callback_timeout_ms",
                "max_memory_bytes",
                "max_callback_invocations",
                "max_vfs_bytes",
                "max_open_host_files",
            )
            if getattr(self, name) is not None
        ]
        if unsupported:
            raise ValueError(
                f"eryx.run sets {', '.join(unsupported)} itself; "
                "only execution_timeout_ms and max_fuel can be set"
            )
        request: dict[str, int] = {}
        if self.execution_timeout_ms is not None:
            request["timeout_secs"] = max(1, -(-self.execution_timeout_ms // 1000))
        if self.max_fuel is not None:
            request["fuel_limit"] = self.max_fuel
        return request
