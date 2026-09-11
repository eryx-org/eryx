"""Eryx CLI: run Python code in a WebAssembly sandbox.

Usage:
    python -m eryx                     # Interactive REPL
    python -m eryx script.py           # Execute a file
    python -m eryx -c 'print("hi")'   # Execute a string
    echo 'print("hi")' | python -m eryx -  # Execute from stdin
    python -m eryx serve               # Start MCP server
    python -m eryx wrap -- npx ...     # Wrap MCP servers

Examples:
    uvx --with pyeryx eryx -c 'import sys; print(sys.version)'
    uvx --with pyeryx eryx --timeout 5000 -c 'print("hello")'
    uvx --with pyeryx eryx --net --allow-host '*.example.com' -c 'import urllib.request; ...'
    uvx --with 'pyeryx[serve]' eryx serve --mcp
    uvx --with 'pyeryx[serve]' eryx wrap -- npx @anthropic/mcp-filesystem .
"""

from __future__ import annotations

import argparse
import sys
import textwrap

import eryx

from eryx._cli import add_sandbox_args, make_mcp_manager, make_net_config, make_resource_limits


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
              eryx serve                        start MCP server
              eryx serve --mcp                  MCP server with inner tools
              eryx wrap -- npx ...              wrap MCP servers
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

    add_sandbox_args(parser)

    return parser


def _write_stdout(chunk: str) -> None:
    """Stream stdout chunks to the terminal in real-time."""
    sys.stdout.write(chunk)
    sys.stdout.flush()


def _write_stderr(chunk: str) -> None:
    """Stream stderr chunks to the terminal in real-time."""
    sys.stderr.write(chunk)
    sys.stderr.flush()


def _run_once(code: str, args: argparse.Namespace, mcp_manager: object | None = None) -> int:
    """Execute code in a stateless Sandbox and print the result."""
    limits = make_resource_limits(args)
    net = make_net_config(args)
    kwargs = {}
    if limits is not None:
        kwargs["resource_limits"] = limits
    if net is not None:
        kwargs["network"] = net
    if args.volume:
        kwargs["volumes"] = args.volume
    if mcp_manager is not None:
        kwargs["mcp"] = mcp_manager

    # Stream output in real-time instead of buffering until completion
    kwargs["on_stdout"] = _write_stdout
    kwargs["on_stderr"] = _write_stderr

    sandbox = eryx.Sandbox(**kwargs)
    try:
        sandbox.execute(code)
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

    return 0


def _repl(args: argparse.Namespace, mcp_manager: object | None = None) -> int:
    """Run an interactive REPL using Session for persistent state."""
    kwargs = {}
    limits = make_resource_limits(args)
    if limits is not None:
        kwargs["execution_timeout_ms"] = limits.execution_timeout_ms
    net = make_net_config(args)
    if net is not None:
        kwargs["network"] = net
    if args.volume:
        kwargs["volumes"] = args.volume
    if mcp_manager is not None:
        kwargs["mcp"] = mcp_manager

    # Stream output in real-time
    kwargs["on_stdout"] = _write_stdout
    kwargs["on_stderr"] = _write_stderr

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
            session.execute(code)
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

    return 0


def main(argv: list[str] | None = None) -> int:
    raw_args = argv if argv is not None else sys.argv[1:]

    # Subcommand: eryx serve
    if raw_args and raw_args[0] == "serve":
        from eryx.serve import serve

        return serve(raw_args[1:])

    # Subcommand: eryx wrap
    if raw_args and raw_args[0] == "wrap":
        from eryx.wrap import wrap

        return wrap(raw_args[1:])

    parser = _build_parser()
    args = parser.parse_args(argv)

    # Create MCP manager if requested
    mcp_manager = make_mcp_manager(args)

    try:
        # -c CODE
        if args.command is not None:
            return _run_once(args.command, args, mcp_manager)

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
            return _run_once(code, args, mcp_manager)

        # interactive REPL (only if stdin is a terminal)
        if sys.stdin.isatty():
            return _repl(args, mcp_manager)

        # piped input without '-' — read stdin anyway
        code = sys.stdin.read()
        if code.strip():
            return _run_once(code, args, mcp_manager)

        parser.print_help()
        return 0
    finally:
        if mcp_manager is not None:
            mcp_manager.close()


if __name__ == "__main__":
    sys.exit(main())
