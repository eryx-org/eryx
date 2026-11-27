#!/usr/bin/env python3
"""Hash the part of Cargo.lock that the given packages can reach.

CI cache keys for the wasm artifacts use this in place of hashing all of
Cargo.lock, so a lock-only bump of a crate the artifact never builds (clap in
the CLI, rmcp in the server) doesn't throw away a 3-12 minute build.

    scripts/lock-closure-hash.py eryx-guest-python eryx-runtime wasmtime

Every version of each named package is a root. The closure follows the lock's
dependency lists, which are already feature-resolved, so it is a superset of
what any one build uses: it can only over-invalidate, never miss a change.

The one exception is the windows-sys family (windows-sys, windows-targets,
windows_*). Cargo re-picks among its versions on unrelated lock bumps, and
the artifacts these keys cover are all built on Linux (the Windows cwasms are
cross-compiled), so those crates are never compiled into them.
"""

import hashlib
import sys
import tomllib
from collections import defaultdict


def main() -> None:
    roots = sys.argv[1:]
    if not roots:
        sys.exit("usage: lock-closure-hash.py PACKAGE...")
    with open("Cargo.lock", "rb") as f:
        packages = tomllib.load(f)["package"]

    by_name = defaultdict(list)
    for p in packages:
        by_name[p["name"]].append(p)

    def resolve(dep: str) -> list[dict]:
        # "name", "name version" or "name version (source)".
        name, _, rest = dep.partition(" ")
        version = rest.split(" ")[0] if rest else None
        return [p for p in by_name[name] if version is None or p["version"] == version]

    missing = [r for r in roots if r not in by_name]
    if missing:
        sys.exit(f"not in Cargo.lock: {', '.join(missing)}")
    seen = {}
    stack = [p for r in roots for p in by_name[r]]
    while stack:
        p = stack.pop()
        if p["name"].startswith("windows"):
            continue
        key = (p["name"], p["version"], p.get("source", ""))
        if key in seen:
            continue
        seen[key] = p
        for dep in p.get("dependencies", []):
            stack.extend(resolve(dep))

    lines = sorted(
        f"{n} {v} {s} {seen[(n, v, s)].get('checksum', '')}" for n, v, s in seen
    )
    print(hashlib.sha256("\n".join(lines).encode()).hexdigest()[:16])


if __name__ == "__main__":
    main()
