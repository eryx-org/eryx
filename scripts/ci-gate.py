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

Labels on the PR (PR_LABELS, comma-separated, fetched live by the workflow)
force groups on: `ci:full` runs every group without calling Claude, and
`ci:<group>` (e.g. `ci:windows`, `ci:feature-combinations`) runs that one.

When no other CI run is in progress or queued (OTHER_CI_RUNS=0, counted by
the workflow), it skips the call and runs everything: the gate exists to
save runner slots, and an idle queue has slots to spare.

Best-effort and fail-open: with no credentials (fork PRs), on any API
error, a refusal or an unparseable answer, every group runs. Pushes to main
never call this and run everything, so a wrong skip surfaces there.

It sends each changed file's path, line counts and diff (capped; lock files
are reduced to the packages whose versions changed), but not the PR title or
body. Only same-repo PRs get an OIDC token, so the diff comes from someone
with write access, and a wrong skip still fails on main.
"""

import json
import os
import re
import sys

# Haiku matched Sonnet 5.5's decisions on past PRs (or ran more when unsure)
# at about a tenth of the cost, ~$0.001 a run. It has no server-side refusal
# fallback; a refusal runs everything.
MODEL = "claude-haiku-5-5"
# $ per million input/output tokens (prompts under 100K), for the cost note.
# A model not listed here reports tokens only.
PRICES = {"claude-haiku-5-5": (0.10, 0.50)}
# More files than this and the PR is broad enough to just run everything.
MAX_FILES = 300
# Diff lines sent per file and in total; past these the model sees counts only.
MAX_PATCH_LINES = 400
MAX_TOTAL_PATCH_LINES = 2000

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
processes, networking, native libraries, cfg(windows)/cfg(unix)). Changes \
to CI workflow files (.github/) only need a group when they change what that \
group's own jobs execute (their steps, runner, toolchain or build commands); \
edits to triggers, job conditions, path filters, caching or other jobs don't. \
Judge from the diffs, not just the file names. Run a group when the diff \
leaves it genuinely unclear. A main-branch run executes everything afterwards, so a wrong skip \
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


def describe(entry: dict, budget: list[int]) -> str:
    """One entry per changed file: its diff (within `budget`), or for lock
    files the packages that moved."""
    line = f"{entry['status']} +{entry['additions']}/-{entry['deletions']} {entry['filename']}"
    if not entry["filename"].endswith(LOCK_FILES):
        patch = (entry.get("patch") or "").splitlines()
        if not patch:
            return line
        if len(patch) > MAX_PATCH_LINES or len(patch) > budget[0]:
            return line + "\n    (diff too large to include)"
        budget[0] -= len(patch)
        return line + "\n```diff\n" + "\n".join(patch) + "\n```"
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


def usage_note(response) -> str:
    """`model, N in / M out, $X` for the decision's annotation."""
    u = response.usage
    note = f"{response.model}, {u.input_tokens} in / {u.output_tokens} out"
    if price := PRICES.get(response.model):
        note += f", ${(u.input_tokens * price[0] + u.output_tokens * price[1]) / 1e6:.4f}"
    return note


def label(group: str) -> str:
    return "ci:" + group.replace("_", "-")


def decide(files: list[str], labels: set[str]) -> tuple[dict[str, bool], str, str]:
    """Returns the decision, its reason, and the call's usage ("" if none)."""
    run_all = {g: True for g in GROUPS}
    if "ci:full" in labels or all(label(g) in labels for g in GROUPS):
        return run_all, "The ci:full label (or every group's label) is set: running everything.", ""
    token_file = os.environ.get("ANTHROPIC_IDENTITY_TOKEN_FILE", "")
    has_federation = os.environ.get("ANTHROPIC_FEDERATION_RULE_ID") and (
        os.path.isfile(token_file) and os.path.getsize(token_file) > 0
    )
    if not (has_federation or os.environ.get("ANTHROPIC_API_KEY")):
        return run_all, "No Anthropic credentials (fork PR?): running everything.", ""
    if os.environ.get("OTHER_CI_RUNS") == "0":
        return run_all, "No other CI runs in progress or queued: running everything.", ""
    if not files:
        return run_all, "No changed files listed: running everything.", ""
    if len(files) > MAX_FILES:
        return run_all, f"{len(files)} files changed: running everything.", ""

    try:
        import anthropic

        client = anthropic.Anthropic(timeout=120, max_retries=2)
        groups = "\n".join(f"- {g}: {d}" for g, d in GROUPS.items())
        response = client.messages.create(
            model=MODEL,
            # Haiku uses up to ~1.5k tokens, thinking included. With the diff cap
            # above this bounds a call at ~40k in + 4k out: under $0.01.
            # Hitting it stops with max_tokens, which runs everything.
            max_tokens=4000,
            output_config={
                "effort": "low",
                "format": {"type": "json_schema", "schema": SCHEMA},
            },
            system=SYSTEM + groups,
            messages=[
                {
                    "role": "user",
                    "content": "Changed files (status +added/-deleted path), each with "
                    "its diff, or for lock files the package versions removed (-) "
                    "and added (+):\n\n"
                    + "\n".join(files),
                }
            ],
        )
        usage = usage_note(response)
        if response.stop_reason != "end_turn":
            return run_all, f"Model stopped with {response.stop_reason}: running everything.", usage
        text = next(b.text for b in response.content if b.type == "text")
        answer = json.loads(text)
        return {g: bool(answer[g]) for g in GROUPS}, answer["reason"], usage
    except Exception as e:  # noqa: BLE001 - every failure fails open
        return run_all, f"Gate failed ({type(e).__name__}: {e}): running everything.", ""


def main() -> None:
    budget = [MAX_TOTAL_PATCH_LINES]
    files = [describe(json.loads(line), budget) for line in sys.stdin if line.strip()]
    labels = {x for x in os.environ.get("PR_LABELS", "").split(",") if x}
    decision, reason, usage = decide(files, labels)
    forced = [g for g in GROUPS if label(g) in labels and not decision[g]]
    for g in forced:
        decision[g] = True
    if forced:
        reason += " Forced on by label: " + ", ".join(label(g) for g in forced) + "."

    oses = [os_ for g, os_ in (("windows", "windows-latest"), ("macos", "macos-latest")) if decision[g]]
    out = [f"{g}={str(v).lower()}" for g, v in decision.items()]
    out.append(f"platform_oses={json.dumps(oses)}")
    with open(os.environ.get("GITHUB_OUTPUT", "/dev/stdout"), "a") as f:
        f.write("\n".join(out) + "\n")

    summary = ["### CI gate", "", reason, ""]
    if usage:
        summary += [f"Claude call: {usage}", ""]
    summary += ["| group | runs |", "|---|---|"]
    summary += [f"| {g} | {'yes' if v else '**skipped**'} |" for g, v in decision.items()]
    summary += [
        "",
        "To run a skipped group, add its label (`ci:windows`, `ci:macos`, `ci:docker`, "
        "`ci:feature-combinations`, `ci:examples`, `ci:book`), or `ci:full` for all, "
        "then re-run all jobs.",
    ]
    with open(os.environ.get("GITHUB_STEP_SUMMARY", "/dev/stderr"), "a") as f:
        f.write("\n".join(summary) + "\n")
    # An annotation on the run and the PR's checks, and the log (the API
    # exposes logs, not summaries).
    skipped = [g for g, v in decision.items() if not v]
    note = f"Skipped {', '.join(skipped)} (add ci:<group> and re-run all jobs to run one)." if skipped else "Running every group."
    if usage:
        note += f" Claude call: {usage}."
    note += f" {reason}"
    # Workflow-command escaping for the message: %, CR and LF.
    note = note.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    print(f"::notice title=CI gate::{note}")
    print(", ".join(out))


if __name__ == "__main__":
    main()
