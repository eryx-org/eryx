#!/usr/bin/env python3
"""Third-party notices for eryx's shipped artifacts (#837).

    scripts/notices.py generate OUTDIR   # write OUTDIR/<artifact>.notices.md
    scripts/notices.py check             # what CI runs; writes nothing

Each artifact's notices are cargo-about's rendering of the Rust crates in its
lock file and targets (about.toml, third_party/about.hbs), followed by the
entries of third_party/manual.toml that list it: code that isn't a Rust crate
in a lock file, such as CPython, wasi-libc and the vendored npm packages.

The host artifacts embed every runtime, so host's notices take in theirs in
full (EMBEDS).

`check` fails, listing every failure, when:
- cargo-about can't accept a crate's licence (it isn't in about.toml's list,
  or can't be determined);
- a vendored input has no manual.toml entry: a file in
  crates/eryx-runtime/libs/ or prebuilt/, a top-level name in the bundled
  Python stdlib, or an npm package in the JS guest's Node modules;
- an entry is stale: a libs/ file whose sha256 changed, a version string
  missing from the library it's recorded for, a crate or CLI version that no
  longer matches Cargo.lock or clis/*.toml, or a WASI SDK other than
  mise.toml's.
It reads the lock files and vendored files only; nothing is built.
"""

import hashlib
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANUAL = ROOT / "third_party" / "manual.toml"
TEMPLATE = ROOT / "third_party" / "about.hbs"

HOST_TARGETS = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
]

# Artifact -> cargo-about arguments for its Rust crates (None: no Rust lock
# of its own here; its crates come from elsewhere, see manual.toml).
ARTIFACTS = {
    # eryx-server, pyeryx and eryx-precompile: the root workspace.
    "host": ["--workspace", *(a for t in HOST_TARGETS for a in ("--target", t))],
    "runtime-python": [
        "-m", "crates/eryx-guest-python/Cargo.toml", "--target", "wasm32-wasip2",
    ],
    "runtime-javascript": [
        "-m", "crates/eryx-guest-js/Cargo.toml", "--target", "wasm32-wasip2",
    ],
    # The WASI 0.3 build, and the WASI 0.2 one js/eryx uses.
    "runtime-shell": [
        "-m", "crates/eryx-guest-shell/Cargo.toml",
        "--target", "wasm32-wasip3", "--target", "wasm32-wasip2",
    ],
    "eryx-bundler": [
        "-m", "crates/eryx-bundler/Cargo.toml", "--target", "wasm32-wasip2",
    ],
    # Built by eryx-build from upstream source (clis/coreutils.toml).
    "coreutils": None,
}

# Artifacts whose notices also take in others' in full, because they embed
# them: the host binaries and packages carry every runtime (the eryx crate's
# `embedded` feature, pyeryx, the server image).
EMBEDS = {
    "host": ["runtime-python", "runtime-javascript", "runtime-shell", "eryx-bundler", "coreutils"],
}

NODE_DIR = ROOT / "crates" / "eryx-guest-js" / "src" / "node"
VENDORED_DIRS = [ROOT / "crates" / "eryx-runtime" / "libs", ROOT / "crates" / "eryx-runtime" / "prebuilt"]
STDLIB = ROOT / "crates" / "eryx" / "python-stdlib.tar.gz"
MISE = ROOT / "mise.toml"


def components():
    return tomllib.loads(MANUAL.read_text())["component"]


def cargo_about(args, output):
    """Render the Rust crates' notices; false if any licence can't be accepted."""
    return subprocess.run(
        [
            "cargo", "about", "generate", "--offline", "--fail",
            "-c", str(ROOT / "about.toml"),
            *args,
            "-o", str(output),
            str(TEMPLATE),
        ],
        cwd=ROOT,
    ).returncode == 0


def manual_section(artifact):
    entries = [c for c in components() if artifact in c["artifacts"]]
    if not entries:
        return ""
    out = ["", "# Other third-party code", ""]
    for c in entries:
        out += [f"## {c['name']} {c['version']}", "", f"Licence: {c['license']}  ", f"Source: {c['url']}", ""]
        for f in c["files"]:
            out += ["```", (ROOT / f).read_text().rstrip(), "```", ""]
    return "\n".join(out)


def fetch():
    """Download the crates cargo-about reads (it runs offline)."""
    manifests = {"Cargo.toml"} | {
        args[args.index("-m") + 1] for args in ARTIFACTS.values() if args and "-m" in args
    }
    for manifest in sorted(manifests):
        subprocess.run(
            ["cargo", "fetch", "--locked", "--manifest-path", manifest], cwd=ROOT, check=True
        )


def generate(outdir):
    """Write every artifact's notices; return the artifacts cargo-about failed on."""
    fetch()
    outdir.mkdir(parents=True, exist_ok=True)
    failed = []
    for artifact, args in ARTIFACTS.items():
        path = outdir / f"{artifact}.notices.md"
        if args is None:
            path.write_text("# Third-party notices\n")
        elif not cargo_about(args, path):
            failed.append(artifact)
            continue
        with path.open("a") as f:
            f.write(manual_section(artifact))
    for artifact, embedded in EMBEDS.items():
        if artifact in failed or any(other in failed for other in embedded):
            continue
        with (outdir / f"{artifact}.notices.md").open("a") as f:
            for other in embedded:
                text = (outdir / f"{other}.notices.md").read_text()
                f.write(f"\n\n# Embedded: {other}\n\n" + text.split("\n", 1)[1])
    return failed


def lock_version(crate):
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    return {p["version"] for p in lock["package"] if p["name"] == crate}


def vendored_npm():
    """The `name@version` lines in the Node modules' vendoring headers."""
    found = {}
    for js in NODE_DIR.glob("*.js"):
        header = re.match(r"/\*!(.*?)\*/", js.read_text(), re.S)
        for name, version in re.findall(r"^(\S+)@(\d+\.\d+\.\d+)$", header[1] if header else "", re.M):
            found[name] = version
    return found


def decompressed(path, cache={}):
    if path not in cache:
        cache[path] = subprocess.run(["zstd", "-dcq", str(ROOT / path)], check=True, capture_output=True).stdout
    return cache[path]


def stdlib_names():
    """Top-level names in the bundled Python stdlib, without `.py`."""
    with tarfile.open(STDLIB) as tar:
        parts = (m.name.split("/") for m in tar.getmembers())
        return {p[1].removesuffix(".py") for p in parts if len(p) > 1 and p[1]}


def check_manual():
    errors = []
    comps = components()
    known = set(ARTIFACTS)
    for c in comps:
        for a in c["artifacts"]:
            if a not in known:
                errors.append(f"{c['name']}: unknown artifact {a!r}")
        for f in c["files"]:
            if not (ROOT / f).is_file():
                errors.append(f"{c['name']}: missing licence file {f}")
        if "crate" in c and c["crate_version"] not in lock_version(c["crate"]):
            errors.append(
                f"{c['name']}: {c['crate']} is {sorted(lock_version(c['crate']))} in Cargo.lock, "
                f"not {c['crate_version']}; update the entry and its licence files"
            )
        if "cli" in c:
            manifest = tomllib.loads((ROOT / c["cli"]).read_text())
            if manifest["source"]["commit"] != c["cli_commit"]:
                errors.append(f"{c['name']}: {c['cli']} builds {manifest['source']['commit']}, not {c['cli_commit']}")

        if "wasi_sdk" in c:
            pinned = tomllib.loads(MISE.read_text())["vars"]["wasi_sdk_version"]
            if c["wasi_sdk"] != pinned:
                errors.append(f"{c['name']}: mise.toml pins {pinned}, not {c['wasi_sdk']}; re-vendored libs need their entries updated")
        for v in c.get("version_strings", []):
            data = decompressed(v["path"])
            for string in v["strings"]:
                if b"\0" + string.encode() + b"\0" not in data:
                    errors.append(f"{c['name']}: {v['path']} doesn't contain {string!r}; update its version")

    covers = {cv["path"]: (c["name"], cv.get("sha256")) for c in comps for cv in c.get("covers", [])}
    for d in VENDORED_DIRS:
        for f in sorted(d.glob("*.zst")):
            rel = str(f.relative_to(ROOT))
            if rel not in covers:
                errors.append(f"{rel} has no entry in third_party/manual.toml")
    for rel, (name, sha) in sorted(covers.items()):
        if not (ROOT / rel).exists():
            errors.append(f"{name}: covers {rel}, which doesn't exist")
        elif sha and hashlib.sha256((ROOT / rel).read_bytes()).hexdigest() != sha:
            errors.append(f"{name}: {rel} has changed since its entry was written; check the entry and update its sha256")

    listed = {n for c in comps for n in c.get("stdlib", [])}
    shipped = stdlib_names()
    for n in sorted(shipped - listed):
        errors.append(f"the bundled stdlib has {n!r}, which no third_party/manual.toml entry lists")
    for n in sorted(listed - shipped):
        errors.append(f"third_party/manual.toml lists {n!r} in the stdlib, which it doesn't have")

    entries = {c["npm"]: c["version"] for c in comps if "npm" in c}
    for name, version in sorted(vendored_npm().items()):
        if entries.get(name) != version:
            errors.append(f"vendored npm package {name}@{version} has no matching entry in third_party/manual.toml")
    return errors


def main():
    match sys.argv[1:]:
        case ["generate", outdir]:
            if failed := generate(Path(outdir)):
                sys.exit(f"cargo-about failed for {', '.join(failed)}; see its output above")
        case ["check"]:
            errors = check_manual()
            with tempfile.TemporaryDirectory() as tmp:
                for artifact in generate(Path(tmp)):
                    errors.append(f"cargo-about failed for {artifact}; see its output above")
            for e in errors:
                print(f"error: {e}", file=sys.stderr)
            if errors:
                sys.exit(1)
        case _:
            sys.exit(__doc__)


if __name__ == "__main__":
    main()
