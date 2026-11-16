#!/usr/bin/env python3
"""Fail unless each built pyeryx package carries eryx's licences and the
third-party notices of what it ships (#1015): in its dist-info/licenses/ (a
wheel) or under its root directory (an sdist).

    scripts/check-wheel-notices.py NOTICES PACKAGE...

NOTICES is the notices file every PACKAGE must hold: host.notices.md for
pyeryx, runtime-<language>.notices.md for pyeryx-runtime-<language>. A
musllinux pyeryx wheel's notices must also carry the GCC runtime library
exception, for the libgcc_s auditwheel bundles in it (#996).
"""

import re
import sys
import tarfile
import zipfile
from pathlib import Path

GCC_EXCEPTION = b"GCC RUNTIME LIBRARY EXCEPTION"
# Text each licence file must hold: a symlink checked out as a file, or any
# other stand-in, fails.
LICENCE_TEXT = {"LICENSE-MIT": b"Permission is hereby granted", "LICENSE-APACHE": b"Apache License"}


def contents(package: Path) -> dict[str, bytes]:
    """The licence files of a wheel or sdist, by path under its licence dir."""
    if package.name.endswith(".whl"):
        with zipfile.ZipFile(package) as z:
            pattern = re.compile(r"^[^/]+\.dist-info/licenses/(.+)$")
            return {m[1]: z.read(n) for n in z.namelist() if (m := pattern.match(n))}
    if package.name.endswith(".tar.gz"):
        with tarfile.open(package) as t:
            pattern = re.compile(r"^[^/]+/(LICENSE-[A-Z]+|notices/[^/]+)$")
            return {
                m[1]: t.extractfile(member).read()
                for member in t.getmembers()
                if member.isfile() and (m := pattern.match(member.name))
            }
    raise SystemExit(f"{package}: not a wheel or sdist")


def problems(package: Path, notices: str) -> list[str]:
    files = contents(package)
    found = []
    for want in ["LICENSE-MIT", "LICENSE-APACHE", f"notices/{notices}"]:
        if want not in files:
            found.append(f"no {want}")
        elif not files[want].strip():
            found.append(f"{want} is empty")
        elif want in LICENCE_TEXT and LICENCE_TEXT[want] not in files[want]:
            found.append(f"{want} isn't the licence text")
    if notices == "host.notices.md" and "musllinux" in package.name:
        if GCC_EXCEPTION not in files.get(f"notices/{notices}", b""):
            found.append("its notices don't carry libgcc_s's GCC runtime library exception")
    return found


def main() -> int:
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    notices, packages = sys.argv[1], [Path(p) for p in sys.argv[2:]]
    failed = False
    for package in packages:
        if found := problems(package, notices):
            failed = True
            for problem in found:
                print(f"::error::{package.name}: {problem}", file=sys.stderr)
        else:
            print(f"{package.name}: notices ok")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
