"""Eryx CLI: run Python code in a WebAssembly sandbox.

Usage:
    python -m eryx                     # Interactive REPL
    python -m eryx script.py           # Execute a file
    python -m eryx -c 'print("hi")'   # Execute a string
    echo 'print("hi")' | python -m eryx -  # Execute from stdin

Examples:
    uvx --with pyeryx eryx -c 'import sys; print(sys.version)'
    uvx --with pyeryx eryx --timeout 5000 -c 'print("hello")'
    uvx --with pyeryx eryx --net --allow-host '*.example.com' -c 'import urllib.request; ...'
"""

from __future__ import annotations

import argparse
import sys
import textwrap

import eryx


def _parse_volume(spec: str) -> tuple[str, str, bool]:
    """Parse a Docker-style volume spec: SRC:DST[:ro|:rw]."""
    parts = spec.split(":")
    if len(parts) == 2:
        return (parts[0], parts[1], False)
    if len(parts) == 3 and parts[2] in ("ro", "rw"):
        return (parts[0], parts[1], parts[2] == "ro")
    raise argparse.ArgumentTypeError(
        f"invalid volume format '{spec}', expected SRC:DST or SRC:DST:ro"
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="eryx",
        description="Run Python code in an Eryx WebAssembly sandbox.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent("""\
            examples:
              eryx                              interactive REPL
              eryx script.py                    execute a file
              eryx -c 'print("hello")'          execute a string
              echo 'print(1+1)' | eryx -        read code from stdin
              eryx --timeout 5000 script.py     set execution timeout
              eryx --net -c 'import requests'   enable network access
        """),
    )

    parser.add_argument(
        "--version",
        action="version",
        version=f"%(prog)s {eryx.__version__}",
    )

    # --- code source (mutually exclusive) ---
    source = parser.add_mutually_exclusive_group()
    source.add_argument(
        "-c",
        metavar="CODE",
        dest="command",
        help="execute CODE and exit",
    )
    source.add_argument(
        "script",
        nargs="?",
        default=None,
        help="Python file to execute (use '-' for stdin)",
    )

    # --- resource limits ---
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

    # --- networking ---
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

    # --- filesystem ---
    fs = parser.add_argument_group("filesystem")
    fs.add_argument(
        "-v",
        "--volume",
        action="append",
        default=[],
        type=_parse_volume,
        metavar="SRC:DST[:ro]",
        help="mount host directory SRC at sandbox path DST (append :ro for read-only)",
    )

    # --- MCP (Model Context Protocol) ---
    mcp_group = parser.add_argument_group("MCP servers")
    mcp_group.add_argument(
        "--mcp",
        action="store_true",
        default=False,
        help="enable MCP server discovery from config files (~/.claude.json, .mcp.json)",
    )
    mcp_group.add_argument(
        "--mcp-config",
        action="append",
        default=[],
        metavar="PATH",
        help="path to MCP config file (can be specified multiple times)",
    )

    return parser


def _make_resource_limits(args: argparse.Namespace) -> eryx.ResourceLimits | None:
    if args.timeout is None and args.max_memory is None:
        return None
    limits = eryx.ResourceLimits()
    if args.timeout is not None:
        limits.execution_timeout_ms = args.timeout
    if args.max_memory is not None:
        limits.max_memory_bytes = args.max_memory
    return limits


def _make_net_config(args: argparse.Namespace) -> eryx.NetConfig | None:
    if not args.net and not args.allow_host:
        return None
    config = eryx.NetConfig.permissive()
    for pattern in args.allow_host:
        config.allow_host(pattern)
    return config


def _run_once(
    code: str,
    args: argparse.Namespace,
    extra_callbacks: list[dict] | None = None,
) -> int:
    """Execute code in a stateless Sandbox and print the result."""
    limits = _make_resource_limits(args)
    net = _make_net_config(args)
    kwargs = {}
    if limits is not None:
        kwargs["resource_limits"] = limits
    if net is not None:
        kwargs["network"] = net
    if args.volume:
        kwargs["volumes"] = args.volume
    if extra_callbacks:
        kwargs["callbacks"] = extra_callbacks

    sandbox = eryx.Sandbox(**kwargs)
    try:
        result = sandbox.execute(code)
    except eryx.TimeoutError as exc:
        print(f"eryx: timeout: {exc}", file=sys.stderr)
        return 1
    except eryx.ResourceLimitError as exc:
        print(f"eryx: resource limit: {exc}", file=sys.stderr)
        return 1
    except eryx.ExecutionError as exc:
        print(f"{exc}", file=sys.stderr)
        return 1
    except eryx.EryxError as exc:
        print(f"eryx: {exc}", file=sys.stderr)
        return 1

    if result.stdout:
        sys.stdout.write(result.stdout)
        if not result.stdout.endswith("\n"):
            sys.stdout.write("\n")
    return 0


def _repl(
    args: argparse.Namespace,
    extra_callbacks: list[dict] | None = None,
) -> int:
    """Run an interactive REPL using Session for persistent state."""
    kwargs = {}
    limits = _make_resource_limits(args)
    if limits is not None:
        kwargs["execution_timeout_ms"] = limits.execution_timeout_ms
    if args.volume:
        kwargs["volumes"] = args.volume
    if extra_callbacks:
        kwargs["callbacks"] = extra_callbacks

    session = eryx.Session(**kwargs)

    print(f"Eryx {eryx.__version__} (sandbox REPL)")
    print('Type "exit()" or Ctrl-D to quit.')

    buf: list[str] = []
    prompt = ">>> "

    while True:
        try:
            line = input(prompt)
        except (EOFError, KeyboardInterrupt):
            if buf:
                buf.clear()
                prompt = ">>> "
                print()
                continue
            print()
            break

        if not buf and line.strip() == "exit()":
            break

        buf.append(line)

        # Detect multi-line blocks: if the line ends with ':', or we're
        # already in a continuation and the line is indented / blank,
        # keep collecting.
        if line.rstrip().endswith(":") or (len(buf) > 1 and (line.startswith((" ", "\t")) or line.strip() == "")):
            prompt = "... "
            continue

        code = "\n".join(buf)
        buf.clear()
        prompt = ">>> "

        try:
            result = session.execute(code)
        except eryx.TimeoutError as exc:
            print(f"TimeoutError: {exc}", file=sys.stderr)
            continue
        except eryx.ResourceLimitError as exc:
            print(f"ResourceLimitError: {exc}", file=sys.stderr)
            continue
        except eryx.ExecutionError as exc:
            print(f"{exc}", file=sys.stderr)
            continue
        except eryx.EryxError as exc:
            print(f"EryxError: {exc}", file=sys.stderr)
            continue

        if result.stdout:
            sys.stdout.write(result.stdout)
            if not result.stdout.endswith("\n"):
                sys.stdout.write("\n")

    return 0


def _setup_mcp(args: argparse.Namespace) -> tuple[list[dict] | None, Any]:
    """Set up MCP servers if --mcp or --mcp-config is used.

    Returns (callbacks, manager) where manager should be closed on exit.
    """
    if not args.mcp and not args.mcp_config:
        return None, None

    from pathlib import Path

    from eryx.mcp import MCPManager

    config_paths = [Path(p) for p in args.mcp_config] if args.mcp_config else None
    manager = MCPManager(config_paths=config_paths)
    manager.discover()

    if not manager.server_names:
        print("eryx: no MCP servers found in config files", file=sys.stderr)
        return None, None

    manager.connect_all()

    if not manager.connected_servers:
        print("eryx: no MCP servers connected successfully", file=sys.stderr)
        manager.close()
        return None, None

    callbacks = manager.as_callbacks()
    if callbacks:
        tool_count = len(callbacks)
        server_count = len(manager.connected_servers)
        print(
            f"eryx: loaded {tool_count} MCP tool(s) from {server_count} server(s)",
            file=sys.stderr,
        )
    return callbacks or None, manager


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)

    # Set up MCP servers if requested
    mcp_callbacks, mcp_manager = _setup_mcp(args)

    try:
        # -c CODE
        if args.command is not None:
            return _run_once(args.command, args, extra_callbacks=mcp_callbacks)

        # script file or stdin
        if args.script is not None:
            if args.script == "-":
                code = sys.stdin.read()
            else:
                try:
                    with open(args.script) as f:
                        code = f.read()
                except FileNotFoundError:
                    print(f"eryx: {args.script}: No such file", file=sys.stderr)
                    return 1
                except OSError as exc:
                    print(f"eryx: {args.script}: {exc}", file=sys.stderr)
                    return 1
            return _run_once(code, args, extra_callbacks=mcp_callbacks)

        # interactive REPL (only if stdin is a terminal)
        if sys.stdin.isatty():
            return _repl(args, extra_callbacks=mcp_callbacks)

        # piped input without '-' — read stdin anyway
        code = sys.stdin.read()
        if code.strip():
            return _run_once(code, args, extra_callbacks=mcp_callbacks)

        parser.print_help()
        return 0
    finally:
        if mcp_manager is not None:
            mcp_manager.close()


if __name__ == "__main__":
    sys.exit(main())
