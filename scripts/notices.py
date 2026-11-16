#!/usr/bin/env python3
"""Third-party notices for eryx's shipped artifacts (#837).

    scripts/notices.py generate OUTDIR   # write OUTDIR/<artifact>.notices.md
    scripts/notices.py check             # what CI runs; writes nothing

Each artifact's notices are cargo-about's rendering of the Rust crates in its
lock file and targets (about.toml, third_party/about.hbs), followed by the
entries of third_party/manual.toml that list it: code that isn't a Rust crate
in a lock file, such as CPython, wasi-libc and the vendored npm packages.

`check` fails when cargo-about can't accept a crate's licence (it isn't in
about.toml's list, or can't be determined), when a vendored input has no
manual.toml entry, or when an entry's version no longer matches what's
vendored. It reads the lock files only; nothing is built.
"""

import re
import subprocess
import sys
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

NODE_DIR = ROOT / "crates" / "eryx-guest-js" / "src" / "node"
LIBS_DIR = ROOT / "crates" / "eryx-runtime" / "libs"


def components():
    return tomllib.loads(MANUAL.read_text())["component"]


def cargo_about(args, output):
    """Render the Rust crates' notices, failing on any licence it can't accept."""
    subprocess.run(
        [
            "cargo", "about", "generate", "--offline", "--fail",
            "-c", str(ROOT / "about.toml"),
            *args,
            "-o", str(output),
            str(TEMPLATE),
        ],
        cwd=ROOT,
        check=True,
    )


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
    fetch()
    outdir.mkdir(parents=True, exist_ok=True)
    for artifact, args in ARTIFACTS.items():
        path = outdir / f"{artifact}.notices.md"
        if args is None:
            path.write_text("# Third-party notices\n")
        else:
            cargo_about(args, path)
        with path.open("a") as f:
            f.write(manual_section(artifact))


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

    covered = {p for c in comps for p in c.get("covers", [])}
    for lib in sorted(LIBS_DIR.glob("*.zst")):
        rel = str(lib.relative_to(ROOT))
        if rel not in covered:
            errors.append(f"{rel} has no entry in third_party/manual.toml")
    for rel in sorted(covered):
        if not (ROOT / rel).exists():
            errors.append(f"manual.toml covers {rel}, which doesn't exist")

    entries = {c["npm"]: c["version"] for c in comps if "npm" in c}
    for name, version in sorted(vendored_npm().items()):
        if entries.get(name) != version:
            errors.append(f"vendored npm package {name}@{version} has no matching entry in third_party/manual.toml")
    return errors


def main():
    match sys.argv[1:]:
        case ["generate", outdir]:
            generate(Path(outdir))
        case ["check"]:
            errors = check_manual()
            with tempfile.TemporaryDirectory() as tmp:
                try:
                    generate(Path(tmp))
                except subprocess.CalledProcessError as e:
                    errors.append(f"{' '.join(e.cmd[:3])} failed; see its output above")
            for e in errors:
                print(f"error: {e}", file=sys.stderr)
            if errors:
                sys.exit(1)
        case _:
            sys.exit(__doc__)


if __name__ == "__main__":
    main()
