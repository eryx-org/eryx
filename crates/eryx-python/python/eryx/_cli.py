"""Shared CLI argument parsing and helpers for eryx commands."""

from __future__ import annotations

import argparse

import eryx


def parse_volume(spec: str) -> tuple[str, str, bool]:
    """Parse a Docker-style volume spec: SRC:DST[:ro|:rw]."""
    parts = spec.split(":")
    if len(parts) == 2:
        return (parts[0], parts[1], False)
    if len(parts) == 3 and parts[2] in ("ro", "rw"):
        return (parts[0], parts[1], parts[2] == "ro")
    raise argparse.ArgumentTypeError(
        f"invalid volume format '{spec}', expected SRC:DST or SRC:DST:ro"
    )


def add_sandbox_args(parser: argparse.ArgumentParser) -> None:
    """Add the standard sandbox configuration argument groups to a parser."""
    limits = parser.add_argument_group("resource limits")
    limits.add_argument(
        "--timeout",
        type=int,
        default=None,
        metavar="MS",
        help="execution timeout in milliseconds (default: 30000)",
    )
    limits.add_argument(
        "--max-memory",
        type=int,
        default=None,
        metavar="BYTES",
        help="maximum memory in bytes (default: 128MB)",
    )

    net = parser.add_argument_group("networking")
    net.add_argument(
        "--net",
        action="store_true",
        default=False,
        help="enable network access",
    )
    net.add_argument(
        "--allow-host",
        action="append",
        default=[],
        metavar="PATTERN",
        help="allow network access to hosts matching PATTERN (implies --net)",
    )

    mcp_group = parser.add_argument_group("MCP (Model Context Protocol)")
    mcp_group.add_argument(
        "--mcp",
        action="store_true",
        default=False,
        help="enable MCP server integration (discovers servers from Claude, Cursor, VS Code, Zed, Windsurf, Codex, Gemini configs)",
    )
    mcp_group.add_argument(
        "--mcp-config",
        action="append",
        default=[],
        metavar="PATH",
        help="path to MCP config file (implies --mcp, can be repeated)",
    )

    fs = parser.add_argument_group("filesystem")
    fs.add_argument(
        "-v",
        "--volume",
        action="append",
        default=[],
        type=parse_volume,
        metavar="SRC:DST[:ro]",
        help="mount host directory SRC at sandbox path DST (append :ro for read-only)",
    )


def make_resource_limits(args: argparse.Namespace) -> eryx.ResourceLimits | None:
    """Build ResourceLimits from parsed CLI args, or None if defaults."""
    if args.timeout is None and args.max_memory is None:
        return None
    limits = eryx.ResourceLimits()
    if args.timeout is not None:
        limits.execution_timeout_ms = args.timeout
    if args.max_memory is not None:
        limits.max_memory_bytes = args.max_memory
    return limits


def make_net_config(args: argparse.Namespace) -> eryx.NetConfig | None:
    """Build NetConfig from parsed CLI args, or None if networking is disabled."""
    if not args.net and not args.allow_host:
        return None
    config = eryx.NetConfig.permissive()
    for pattern in args.allow_host:
        config.allow_host(pattern)
    return config


def make_mcp_manager(args: argparse.Namespace) -> object | None:
    """Create an MCP manager if --mcp or --mcp-config is specified."""
    if not args.mcp and not args.mcp_config:
        return None

    from eryx.mcp import connect_servers

    config_paths = args.mcp_config if args.mcp_config else None
    return connect_servers(config_paths=config_paths)
