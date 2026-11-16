#!/usr/bin/env python3
"""Third-party notices for eryx's shipped artifacts (#837).

    scripts/notices.py generate OUTDIR   # write OUTDIR/<artifact>.notices.md
    scripts/notices.py check             # what CI runs; writes nothing

Each artifact's notices are cargo-about's rendering of the Rust crates in its
lock file and targets (about.toml, third_party/about.hbs), followed by the
entries of third_party/manual.toml that list it: code that isn't a Rust crate
in a lock file, such as CPython, wasi-libc and the vendored npm packages.

A Rust artifact also links the standard library its toolchain builds (STD):
std's own dependencies are rendered the same way, from the toolchain's
rust-src (`library/`, whose Cargo.lock pins them and whose vendor/ holds
their sources). That needs rust-src installed for each toolchain, which the
`notices` and `notices-check` mise tasks do first.

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
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANUAL = ROOT / "third_party" / "manual.toml"
TEMPLATE = ROOT / "third_party" / "about.hbs"
STD_ABOUT = ROOT / "third_party" / "about-std.toml"

HOST_TARGETS = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    # The musllinux pyeryx wheels.
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
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

# The toolchains the Rust artifacts are built with.
def stable_toolchain():
    """The pinned stable toolchain. rust-toolchain.toml pins it for cargo,
    and mise.toml's `[tools] rust` (RUSTUP_TOOLCHAIN in `mise run`) for the
    mise tasks; they must agree, or the notices could describe a toolchain
    a build doesn't use."""
    channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    tool = tomllib.loads((ROOT / "mise.toml").read_text())["tools"]["rust"]
    mise_version = tool["version"] if isinstance(tool, dict) else tool
    if channel != mise_version:
        sys.exit(f"rust-toolchain.toml pins {channel} but mise.toml's [tools] rust is {mise_version}; make them agree")
    return channel


STABLE = stable_toolchain()
GUEST = tomllib.loads((ROOT / "mise.toml").read_text())["vars"]["guest_rust_toolchain"]

# Artifact -> the toolchain and targets of the standard library it links.
STD = {
    "host": (STABLE, HOST_TARGETS),
    # Built with -Z build-std for wasm32-wasip1 (crates/eryx-runtime/build.rs).
    "runtime-python": (GUEST, ["wasm32-wasip1"]),
    "runtime-javascript": (GUEST, ["wasm32-wasip1"]),
    "runtime-shell": (STABLE, ["wasm32-wasip3", "wasm32-wasip2"]),
    "eryx-bundler": (STABLE, ["wasm32-wasip2"]),
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


def pinned_cargo_about():
    """The cargo-about binary mise.toml pins, checked to be that version.

    `cargo about` would run whichever cargo-about $CARGO_HOME/bin has first,
    whatever mise pins, and an older one renders different notices (0.7.1
    drops wasm32-wasip3's crates and ignores some clarifications)."""
    pinned = tomllib.loads(MISE.read_text())["tools"]["cargo:cargo-about"]["version"]
    # `mise where`, not `mise which`: the latter returns a cargo-about found
    # in $CARGO_HOME/bin first.
    found = subprocess.run(["mise", "where", "cargo:cargo-about"], cwd=ROOT, capture_output=True, text=True)
    binary = Path(found.stdout.strip()) / "bin" / "cargo-about"
    if found.returncode != 0 or not binary.is_file():
        sys.exit(f"can't find mise's cargo-about {pinned}: {found.stderr.strip()} (run `mise install`)")
    version = run([str(binary), "--version"]).split()[-1]
    if version != pinned:
        sys.exit(f"{binary} is cargo-about {version}, but mise.toml pins {pinned}")
    return binary


def run(command, **kwargs):
    """Run a command whose output we read, showing its stderr if it fails."""
    result = subprocess.run(command, capture_output=True, text=True, **kwargs)
    if result.returncode != 0:
        sys.exit(f"{' '.join(command)} failed ({result.returncode}):\n{result.stderr}")
    return result.stdout


def cargo_about(args, output, label, cwd=ROOT, env=None, config=ROOT / "about.toml"):
    """Render the Rust crates' notices; false if any licence can't be accepted,
    or cargo-about leaves out a crate cargo builds that can't be accounted for
    otherwise (see missed_section). `label` names what they're the crates of."""
    binary = pinned_cargo_about()

    def run_about(extra, out):
        return subprocess.run(
            [str(binary), "about", "generate", "--offline", "--fail", "-c", str(config), *args, *extra, "-o", str(out)],
            cwd=cwd,
            env=env,
        ).returncode == 0

    if not run_about([str(TEMPLATE)], output):
        return False
    with tempfile.TemporaryDirectory() as tmp:
        listed = Path(tmp) / "crates.json"
        if not run_about(["--format", "json"], listed):
            return False
        about = {f"{c['package']['name']} {c['package']['version']}" for c in json.loads(listed.read_text())["crates"]}
    cargo = tree_crates(args, cwd, env)
    # cargo-about resolves targets and features itself, and can disagree with
    # cargo (0.7.1 dropped every crate only wasm32-wasip3 uses, and missed
    # some reached through weak `dep?/feature`s). A crate it lists that cargo
    # doesn't build only over-states the notices.
    for name in sorted(about - cargo):
        print(f"warning: cargo-about listed {name}, which cargo doesn't build for {args}", file=sys.stderr)
    missed = sorted(cargo - about)
    if missed:
        section = missed_section(missed, args, label, cwd, env)
        if section is None:
            return False
        with output.open("a") as f:
            f.write(section)
    return True


LICENCE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|UNLICENSE|NOTICE|COPYRIGHT)", re.I)


def accepted_licence(expression):
    """Whether every licence in an SPDX expression is one about.toml accepts."""
    accepted = set(tomllib.loads((ROOT / "about.toml").read_text())["accepted"])
    names = re.sub(r"[()]", " ", expression).split()
    ids = [n for i, n in enumerate(names) if n not in ("AND", "OR", "WITH") and (i == 0 or names[i - 1] != "WITH")]
    exceptions = [names[i + 1] for i, n in enumerate(names) if n == "WITH"]
    return all(i in accepted for i in ids) and all(
        any(a.endswith(f" WITH {e}") for a in accepted) for e in exceptions
    )


def missed_section(missed, args, label, cwd, env):
    """Notices for crates cargo builds but cargo-about left out, from each
    crate's declared licence and its licence files; None (after reporting
    why) if one can't be accounted for."""
    manifest = args[args.index("-m") + 1] if "-m" in args else str(ROOT / "Cargo.toml")
    metadata = json.loads(run(
        ["cargo", "metadata", "--format-version", "1", "--offline", "--manifest-path", manifest], cwd=cwd, env=env,
    ))
    packages = {f"{p['name']} {p['version']}": p for p in metadata["packages"]}
    out = ["", f"## Crates of {label} that cargo-about left out", "",
           "cargo builds these, but cargo-about didn't list them.", ""]
    ok = True
    for name in missed:
        package = packages[name]
        licence = package.get("license")
        files = sorted(f for f in Path(package["manifest_path"]).parent.iterdir() if LICENCE_FILE.match(f.name) and f.is_file())
        if not licence or not accepted_licence(licence) or not files:
            print(f"error: cargo-about left out {name} (licence {licence!r}, {len(files)} licence files), which cargo builds for {args}", file=sys.stderr)
            ok = False
            continue
        out += [f"### {name}", "", f"Licence: {licence}  ", f"Source: {package.get('repository') or 'crates.io'}", ""]
        for f in files:
            out += ["```", f.read_text(errors="replace").rstrip(), "```", ""]
    return "\n".join(out) if ok else None


def tree_crates(args, cwd, env):
    """The crates cargo itself builds for cargo-about's arguments: `cargo
    tree -e normal` of the root package (or with --workspace every member)
    on each target, with the same features. (`cargo metadata` would do too
    much: its resolve unifies features across the workspace and dev
    dependencies.)"""
    manifest = args[args.index("-m") + 1] if "-m" in args else str(ROOT / "Cargo.toml")
    targets = [args[i + 1] for i, a in enumerate(args) if a == "--target"]
    flags = ["--workspace"] if "--workspace" in args else []
    for i, a in enumerate(args):
        if a in ("--no-default-features", "--all-features"):
            flags.append(a)
        elif a == "--features":
            flags += [a, args[i + 1]]
    if not targets:
        sys.exit(f"no --target in {args}: the cross-check needs the artifact's targets")
    crates = set()
    for target in targets:
        lines = run(
            ["cargo", "tree", "--offline", "-e", "normal", "--target", target, "--prefix", "none",
             "--format", "{p}", *flags, "--manifest-path", manifest],
            cwd=cwd, env=env,
        ).splitlines()
        # "name vX.Y.Z (path)" or "name vX.Y.Z", "(*)" when already shown.
        crates |= {f"{line.split()[0]} {line.split()[1].removeprefix('v')}" for line in lines if line.strip()}
    if not crates:
        sys.exit(f"cargo tree found no crates for {args}")
    return crates


def std_library(toolchain):
    """The rust-src `library/` directory of `toolchain`."""
    sysroot = subprocess.run(
        ["rustc", f"+{toolchain}", "--print", "sysroot"], capture_output=True, text=True, check=True
    ).stdout.strip()
    library = Path(sysroot) / "lib" / "rustlib" / "src" / "rust" / "library"
    if not (library / "Cargo.lock").is_file():
        sys.exit(f"{toolchain} has no rust-src; run `rustup component add rust-src --toolchain {toolchain}`")
    return library


def std_overlay(library, tmp):
    """A copy of `library` (symlinks to its files) whose own crates all
    declare a licence.

    The Rust project's crates in library/ are under its MIT OR Apache-2.0
    (its COPYRIGHT and REUSE.toml), but a few internal ones (sysroot, test,
    windows-link) don't say so, and cargo-about can't clarify a crate without
    a licence file. Some files in them are under other licences, which the
    "Rust standard library" entries in manual.toml carry: core's Unicode
    tables (Unicode-3.0, REUSE.toml). Cargo.toml files and the lock file are
    real copies, so nothing writes into the toolchain.
    """
    overlay = Path(tmp) / "library"
    shutil.copytree(library, overlay, copy_function=os.symlink, ignore=shutil.ignore_patterns("vendor", "Cargo.lock"))
    shutil.copy(library / "Cargo.lock", overlay / "Cargo.lock")
    # The sources rust-src vendors, which its .cargo/config.toml points at.
    os.symlink(library / "vendor", overlay / "vendor")
    for toml in overlay.rglob("Cargo.toml"):
        text = toml.read_text()
        package = tomllib.loads(text).get("package")
        toml.unlink()
        if package and not {"license", "license-file"} & package.keys():
            text = text.replace("[package]\n", '[package]\nlicense = "MIT OR Apache-2.0"\n', 1)
        toml.write_text(text)
    return overlay


def std_section(artifact, path):
    """Append the standard library's notices for `artifact` to `path`, if it
    links one; false if cargo-about fails."""
    if artifact not in STD:
        return True
    toolchain, targets = STD[artifact]
    with tempfile.TemporaryDirectory() as tmp:
        library = std_overlay(std_library(toolchain), tmp)
        std = library / "std"
        # std's workspace uses unstable cargo features, which RUSTC_BOOTSTRAP
        # lets its toolchain's cargo read. Everything runs from std's
        # directory, so rust-src's .cargo/config.toml replaces crates.io with
        # the sources it vendors (cargo doesn't take source replacement from
        # the environment), and offline works with an empty cargo cache.
        env = {**os.environ, "RUSTUP_TOOLCHAIN": toolchain, "RUSTC_BOOTSTRAP": "1"}
        # about.toml plus std's own clarifications, which would only warn
        # (as config for a crate that isn't there) on every other artifact.
        config = Path(tmp) / "about.toml"
        config.write_text((ROOT / "about.toml").read_text() + "\n" + STD_ABOUT.read_text())
        rendered = Path(tmp) / "std.md"
        # The features the toolchain's own std builds use (and more than a
        # -Z build-std one may need), so nothing it can link is left out.
        args = [
            "-m", str(std / "Cargo.toml"), "--features", "backtrace panic-unwind",
            *(a for t in targets for a in ("--target", t)),
        ]
        if not cargo_about(args, rendered, "the Rust standard library", cwd=std, env=env, config=config):
            return False
        version = subprocess.run(
            ["rustc", f"+{toolchain}", "--version"], capture_output=True, text=True, check=True
        ).stdout.strip()
        with path.open("a") as f:
            f.write(f"\n\n# The Rust standard library ({version})\n\n" + rendered.read_text().split("\n", 1)[1])
    return True


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
        elif not cargo_about(args, path, artifact):
            failed.append(artifact)
            continue
        if not std_section(artifact, path):
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

        if "rust_toolchain" in c and c["rust_toolchain"] not in (STABLE, GUEST):
            errors.append(
                f"{c['name']}: the pinned toolchains are {STABLE} (rust-toolchain.toml) and {GUEST} "
                f"(mise.toml), not {c['rust_toolchain']}; update the entry and its licence files"
            )
        if "sysroot_strings" in c:
            sysroot = Path(subprocess.run(
                ["rustc", f"+{c['rust_toolchain']}", "--print", "sysroot"],
                capture_output=True, text=True, check=True,
            ).stdout.strip())
            for rel in c["sysroot_strings"]["paths"]:
                if not (sysroot / rel).is_file():
                    errors.append(
                        f"{c['name']}: {c['rust_toolchain']} has no {rel}; install its target "
                        f"(`mise run ensure-rust-src` adds them)"
                    )
                    continue
                data = (sysroot / rel).read_bytes()
                for string in c["sysroot_strings"]["strings"]:
                    if string.encode() not in data:
                        errors.append(f"{c['name']}: {c['rust_toolchain']}'s {rel} doesn't contain {string!r}; update the entry")
        if "wasi_sdk" in c:
            pinned = tomllib.loads(MISE.read_text())["vars"]["wasi_sdk_version"]
            if c["wasi_sdk"] != pinned:
                errors.append(f"{c['name']}: mise.toml pins {pinned}, not {c['wasi_sdk']}; re-vendored libs need their entries updated")
        for v in c.get("version_strings", []):
            data = decompressed(v["path"])
            for string in v["strings"]:
                if b"\0" + string.encode() + b"\0" not in data:
                    errors.append(f"{c['name']}: {v['path']} doesn't contain {string!r}; update its version")

    # Each Rust artifact's toolchain needs an entry with its licence texts.
    for artifact, (toolchain, _) in STD.items():
        if not any(artifact in c["artifacts"] and c.get("rust_toolchain") == toolchain for c in comps):
            errors.append(f"{artifact}: no third_party/manual.toml entry for the Rust toolchain {toolchain}")

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
