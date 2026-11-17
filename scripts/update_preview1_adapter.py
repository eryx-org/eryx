#!/usr/bin/env python3
"""Keep the vendored WASI preview 1 command adapter in step with wasmtime.

    scripts/update_preview1_adapter.py      # or: mise run update-preview1-adapter

eryx adapts preview 1 command modules into components with the adapter from
the wasmtime release it links (#993):
crates/eryx/adapters/wasi_snapshot_preview1.command.wasm, listed in
third_party/manual.toml. For the wasmtime version in Cargo.lock, this:

- downloads that release's `wasi_snapshot_preview1.command.wasm` if the
  vendored one isn't it, checks it against the release's sha256 (GitHub's
  asset digest), and updates the manual.toml entry's version, URL,
  crate_version and sha256;
- fails if wasmtime's LICENSE changed (third_party/licenses/wasmtime-LICENSE
  is updated by hand, after reading what changed);
- fails if the `wit-component-wasmtime` pin no longer matches wasmtime's own
  wit-component, or `wasmtime-environ` isn't pinned to that wasmtime.

Nothing changes when all of it is already in step. GH_TOKEN, if set, is
used for GitHub API requests. Only the standard library is used.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import tomllib
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ADAPTER = ROOT / "crates/eryx/adapters/wasi_snapshot_preview1.command.wasm"
ASSET = ADAPTER.name
MANUAL = ROOT / "third_party/manual.toml"
LICENSE = ROOT / "third_party/licenses/wasmtime-LICENSE"
ENTRY = "wasi_snapshot_preview1 command adapter (Wasmtime)"
REPO = "bytecodealliance/wasmtime"


class Error(Exception):
    pass


def fetch(url: str, accept: str | None = None) -> bytes:
    request = urllib.request.Request(url)
    if accept:
        request.add_header("Accept", accept)
    token = os.environ.get("GH_TOKEN")
    if token and url.startswith("https://api.github.com/"):
        request.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(request) as response:
        return response.read()


def locked_version(lock: dict, name: str) -> str:
    versions = sorted({p["version"] for p in lock["package"] if p["name"] == name})
    if len(versions) != 1:
        raise Error(f"Cargo.lock has {len(versions)} versions of {name}: {versions}")
    return versions[0]


def entry_block(text: str, name: str) -> tuple[int, int]:
    """The span of the `[[component]]` block named `name` in manual.toml."""
    starts = [m.start() for m in re.finditer(r"^\[\[component\]\]$", text, re.M)]
    for i, start in enumerate(starts):
        end = starts[i + 1] if i + 1 < len(starts) else len(text)
        if re.search(rf'^name = "{re.escape(name)}"$', text[start:end], re.M):
            return start, end
    raise Error(f"no [[component]] named {name!r} in {MANUAL.relative_to(ROOT)}")


def update_entry(text: str, version: str, sha256: str) -> str:
    """manual.toml's text with the adapter entry describing `version`."""
    start, end = entry_block(text, ENTRY)
    block = text[start:end]
    for pattern, replacement in [
        (r'^version = "[^"]*"$', f'version = "{version}"'),
        (r'^url = "[^"]*"$', f'url = "https://github.com/{REPO}/releases/tag/v{version}"'),
        (r'^crate_version = "[^"]*"$', f'crate_version = "{version}"'),
        (r'(\bpath = "crates/eryx/adapters/' + re.escape(ASSET) + r'", sha256 = ")[0-9a-f]+(")',
         rf"\g<1>{sha256}\g<2>"),
    ]:
        block, count = re.subn(pattern, replacement, block, count=1, flags=re.M)
        if count != 1:
            raise Error(f"{ENTRY}: no line matching {pattern!r} to update")
    return text[:start] + block + text[end:]


def check_pins(cargo: dict, wasmtime: str, upstream: dict) -> list[str]:
    """Problems with the pins that must follow wasmtime."""
    problems = []
    deps = cargo["workspace"]["dependencies"]
    ours = deps["wit-component-wasmtime"]["version"].lstrip("=")
    theirs = upstream["workspace"]["dependencies"]["wit-component"]
    theirs = theirs if isinstance(theirs, str) else theirs["version"]
    if ours != theirs.lstrip("="):
        problems.append(
            f"Cargo.toml pins wit-component-wasmtime to ={ours}, but wasmtime {wasmtime} "
            f"uses wit-component {theirs}: set it to ={theirs} and re-lock"
        )
    environ = deps["wasmtime-environ"]
    environ = environ if isinstance(environ, str) else environ["version"]
    if environ != f"={wasmtime}":
        problems.append(
            f"Cargo.toml pins wasmtime-environ to {environ}, but Cargo.lock has wasmtime "
            f"{wasmtime}: set it to ={wasmtime} (process/vmctx.rs mirrors its layout)"
        )
    return problems


def main() -> int:
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    wasmtime = locked_version(lock, "wasmtime")
    tag = f"v{wasmtime}"
    problems = []

    release = json.loads(fetch(f"https://api.github.com/repos/{REPO}/releases/tags/{tag}"))
    asset = next((a for a in release["assets"] if a["name"] == ASSET), None)
    if asset is None or not str(asset.get("digest", "")).startswith("sha256:"):
        raise Error(f"wasmtime {tag} has no {ASSET} with a sha256 digest")
    sha256 = asset["digest"].removeprefix("sha256:")

    changed = []
    if hashlib.sha256(ADAPTER.read_bytes()).hexdigest() != sha256:
        data = fetch(asset["browser_download_url"])
        if hashlib.sha256(data).hexdigest() != sha256:
            raise Error(f"the downloaded {ASSET} doesn't match {tag}'s sha256 {sha256}")
        ADAPTER.write_bytes(data)
        changed.append(str(ADAPTER.relative_to(ROOT)))
    text = MANUAL.read_text()
    updated = update_entry(text, wasmtime, sha256)
    if updated != text:
        MANUAL.write_text(updated)
        changed.append(str(MANUAL.relative_to(ROOT)))

    license_text = fetch(f"https://raw.githubusercontent.com/{REPO}/{tag}/LICENSE")
    if license_text != LICENSE.read_bytes():
        problems.append(
            f"wasmtime {tag}'s LICENSE differs from {LICENSE.relative_to(ROOT)}: read what "
            "changed, then update it (and the entries that list it) by hand"
        )
    upstream = tomllib.loads(
        fetch(f"https://raw.githubusercontent.com/{REPO}/{tag}/Cargo.toml").decode()
    )
    problems += check_pins(tomllib.loads((ROOT / "Cargo.toml").read_text()), wasmtime, upstream)

    for path in changed:
        print(f"updated {path} for wasmtime {wasmtime}")
    if not changed:
        print(f"the preview1 adapter is in step with wasmtime {wasmtime}")
    for problem in problems:
        print(f"error: {problem}", file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (Error, OSError) as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
