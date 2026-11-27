#!/usr/bin/env python3
"""Ask Claude which optional CI job groups a pull request needs.

Reads the PR's changed files on stdin as JSON lines, one object per file
from the GitHub pull request files API, and writes one GITHUB_OUTPUT
line per group (`windows=true` ...) plus `platform_oses`, the JSON list of
runners for the platform-check matrix.

Authenticates with workload identity federation: the workflow writes a
GitHub OIDC token to ANTHROPIC_IDENTITY_TOKEN_FILE and the SDK exchanges it
(with ANTHROPIC_FEDERATION_RULE_ID and ANTHROPIC_ORGANIZATION_ID) for a
short-lived API token, so no API key is stored in the repo.

Best-effort and fail-open: with no credentials (fork PRs), on any API
error, a refusal or an unparseable answer, every group runs. Pushes to main
never call this and run everything, so a wrong skip surfaces there.

Only file paths, line counts and, for lock files, the packages whose
versions changed are sent, not the diff or the PR text: they are what the
decision needs, and the PR title and body are free text anyone opening a PR
controls.
"""

import json
import os
import re
import sys

MODEL = "claude-opus-5-5"
# More files than this and the PR is broad enough to just run everything.
MAX_FILES = 300

# Group -> what its jobs check. The model sees these descriptions.
GROUPS = {
    "windows": "Windows runners: `cargo check` of the workspace on Windows, and "
    "building pyeryx (crates/eryx-python, PyO3/maturin) and running its pytest "
    "suite on Windows.",
    "macos": "macOS runners: the same two checks on macOS (arm64).",
    "docker": "Building the eryx-server Docker images (crates/eryx-server/Dockerfile, "
    "amd64 and arm64, all-languages and Python-only variants).",
    "feature_combinations": "`cargo check` of the eryx crate under every "
    "combination of its cargo features.",
    "examples": "Running the Rust examples in crates/eryx/examples.",
    "book": "Building the mdBook docs (book/) and running their code samples.",
}

SYSTEM = """\
You decide which optional CI job groups a pull request to the eryx repository \
needs. Eryx is a Rust library that runs Python, JavaScript and shell guests \
compiled to WebAssembly in a wasmtime sandbox. Layout: crates/eryx (the main \
library), crates/eryx-runtime (wasm runtime packaging, WIT in wit/), \
crates/eryx-guest-* (guest code compiled to wasm32, built on Linux only), \
crates/eryx-vfs (virtual filesystem), crates/eryx-server (gRPC server, shipped \
as a Linux Docker image), crates/eryx-python (Python bindings, wheels for \
Linux/macOS/Windows), js/ (JavaScript bindings), sdk/ (Python and TypeScript \
client SDKs), book/ (mdBook docs), .github/ (CI).

Lint, unit and integration tests, and the Linux Python bindings tests always \
run on Linux and are not your concern. For each optional group, say whether \
this change could plausibly break something that group would catch and the \
Linux jobs would not. Skip a group when the change can't reach it: for \
example, guest wasm code, the server, the book, SDKs or JS bindings can't break a \
Windows or macOS host build, and a dependency bump in Cargo.lock only matters \
to a platform group if the crate has platform-specific code (filesystem, \
processes, networking, native libraries, cfg(windows)/cfg(unix)). Run a group \
when unsure. A main-branch run executes everything afterwards, so a wrong skip \
is caught there, but costs a broken main.

Groups:
"""

SCHEMA = {
    "type": "object",
    "properties": {
        **{g: {"type": "boolean"} for g in GROUPS},
        "reason": {"type": "string", "description": "One or two sentences."},
    },
    "required": [*GROUPS, "reason"],
    "additionalProperties": False,
}


LOCK_FILES = ("Cargo.lock", "package-lock.json", "uv.lock")
# `name = "x"` / `version = "y"` (Cargo.lock, uv.lock) or `"node_modules/x": {`.
NAME = re.compile(r'^.\s*name = "([^"]+)"')
VERSION = re.compile(r'^([-+])\s*version = "([^"]+)"')
NODE = re.compile(r'^([-+])\s*"node_modules/([^"]+)"')


def describe(entry: dict) -> str:
    """One line per changed file; lock files also list the packages that moved."""
    line = f"{entry['status']} +{entry['additions']}/-{entry['deletions']} {entry['filename']}"
    if not entry["filename"].endswith(LOCK_FILES):
        return line
    moved, name = set(), None
    for raw in (entry.get("patch") or "").splitlines():
        # The package name is usually an unchanged context line above the
        # changed version, so track it from every line.
        if m := NAME.match(raw):
            name = m.group(1)
        elif (m := VERSION.match(raw)) and name:
            moved.add(f"{m.group(1)}{name} {m.group(2)}")
        elif m := NODE.match(raw):
            moved.add(f"{m.group(1)}{m.group(2)}")
    if moved:
        line += "\n    packages: " + ", ".join(sorted(moved)[:200])
    return line


def decide(files: list[str]) -> tuple[dict[str, bool], str]:
    run_all = {g: True for g in GROUPS}
    token_file = os.environ.get("ANTHROPIC_IDENTITY_TOKEN_FILE", "")
    has_federation = os.environ.get("ANTHROPIC_FEDERATION_RULE_ID") and (
        os.path.isfile(token_file) and os.path.getsize(token_file) > 0
    )
    if not (has_federation or os.environ.get("ANTHROPIC_API_KEY")):
        return run_all, "No Anthropic credentials (fork PR?): running everything."
    if not files:
        return run_all, "No changed files listed: running everything."
    if len(files) > MAX_FILES:
        return run_all, f"{len(files)} files changed: running everything."

    try:
        import anthropic

        client = anthropic.Anthropic(timeout=120, max_retries=2)
        groups = "\n".join(f"- {g}: {d}" for g, d in GROUPS.items())
        response = client.beta.messages.create(
            model=MODEL,
            max_tokens=8000,
            betas=["server-side-fallback-2026-07-01"],
            fallbacks="default",
            output_config={
                "effort": "low",
                "format": {"type": "json_schema", "schema": SCHEMA},
            },
            system=SYSTEM + groups,
            messages=[
                {
                    "role": "user",
                    "content": "Changed files (status +added/-deleted path; for lock files, "
                    "the package versions removed (-) and added (+)):\n"
                    + "\n".join(files),
                }
            ],
        )
        if response.stop_reason != "end_turn":
            return run_all, f"Model stopped with {response.stop_reason}: running everything."
        text = next(b.text for b in response.content if b.type == "text")
        answer = json.loads(text)
        return {g: bool(answer[g]) for g in GROUPS}, answer["reason"]
    except Exception as e:  # noqa: BLE001 - every failure fails open
        return run_all, f"Gate failed ({type(e).__name__}: {e}): running everything."


def main() -> None:
    files = [describe(json.loads(line)) for line in sys.stdin if line.strip()]
    decision, reason = decide(files)

    oses = [os_ for g, os_ in (("windows", "windows-latest"), ("macos", "macos-latest")) if decision[g]]
    out = [f"{g}={str(v).lower()}" for g, v in decision.items()]
    out.append(f"platform_oses={json.dumps(oses)}")
    with open(os.environ.get("GITHUB_OUTPUT", "/dev/stdout"), "a") as f:
        f.write("\n".join(out) + "\n")

    summary = ["### CI gate", "", reason, "", "| group | runs |", "|---|---|"]
    summary += [f"| {g} | {'yes' if v else '**skipped**'} |" for g, v in decision.items()]
    summary += ["", "Add the `ci:full` label to run every group."]
    with open(os.environ.get("GITHUB_STEP_SUMMARY", "/dev/stderr"), "a") as f:
        f.write("\n".join(summary) + "\n")
    # The log too: the API exposes job logs but not job summaries.
    print(reason)
    print(", ".join(out))


if __name__ == "__main__":
    main()
