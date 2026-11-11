"""Serving local directories to a run, read-only, over ``/v1/stream``.

The sandboxed code can ask for any path, and it may be untrusted, so this is
the security boundary: paths are checked here, not by eryx.run.
"""

from __future__ import annotations

import fnmatch
import os
import stat as stat_module
from collections.abc import Sequence
from typing import Any, Optional

#: Files and directories a mount doesn't serve unless ``hidden=`` says
#: otherwise. A pattern without ``/`` matches any path component; one with
#: ``/`` matches a path from the mount's root, or a directory above it.
#: Matching ignores case, as the filesystem may (``.ENV`` opens ``.env`` on
#: macOS).
DEFAULT_HIDDEN: tuple[str, ...] = (
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.tfstate",
    "*.tfstate.*",
    "*.tfvars",
    ".pgpass",
    ".htpasswd",
    ".kube",
    "id_rsa*",
    "id_ecdsa*",
    "id_ed25519*",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".aws",
    ".ssh",
    ".gnupg",
    ".docker",
    ".git/config",
    ".git-credentials",
)

#: Files up to this size come with their directory's listing
INLINE_MAX = 64 * 1024
#: Most file data one listing sends; the rest is read when the code asks
INLINE_TOTAL = 1024 * 1024
#: Most one read sends, whatever it asks for
READ_MAX = 4 * 1024 * 1024

#: Guest paths are under this
_MOUNT_ROOT = "/mnt/"


class _Refused(Exception):
    """An operation the client won't do, with the API's error code."""

    def __init__(self, code: str, message: str = "") -> None:
        super().__init__(message)
        self.code = code


def _hidden(rel: str, patterns: Sequence[str]) -> bool:
    parts = rel.lower().split("/")
    for pattern in (p.lower() for p in patterns):
        if "/" in pattern:
            if any(fnmatch.fnmatchcase("/".join(parts[:i]), pattern) for i in range(1, len(parts) + 1)):
                return True
        elif any(fnmatch.fnmatchcase(part, pattern) for part in parts):
            return True
    return False


class Mount:
    """One local directory, served read-only at a guest path."""

    def __init__(self, host_path: str, guest_path: str, hidden: Sequence[str]) -> None:
        self.root = os.path.realpath(host_path)
        if not os.path.isdir(self.root):
            raise ValueError(f"volume {host_path!r} isn't a directory")
        self.guest_path = guest_path
        if isinstance(hidden, str):
            raise TypeError("hidden= is a list of patterns, e.g. ['.env']")
        self.hidden = tuple(hidden)

    def resolve(self, rel: str) -> str:
        """The host path for ``rel`` (relative to the mount), refusing what it
        mustn't serve: ``..``, symlinks out of the mount, and hidden files."""
        if rel and (
            rel.startswith("/") or "\0" in rel or any(p in ("", ".", "..") for p in rel.split("/"))
        ):
            raise _Refused("permission_denied", f"bad path {rel!r}")
        if rel and _hidden(rel, self.hidden):
            raise _Refused("not_found")
        real = os.path.realpath(os.path.join(self.root, rel))
        if os.path.commonpath([self.root, real]) != self.root:
            raise _Refused("not_found")
        # A symlink inside the mount mustn't lead to a hidden file either
        inner = os.path.relpath(real, self.root).replace(os.sep, "/")
        if inner != "." and _hidden(inner, self.hidden):
            raise _Refused("not_found")
        return real

    def stat(self, rel: str) -> dict[str, Any]:
        return _stat(os.stat(self.resolve(rel)))

    def list(self, rel: str) -> tuple[list[dict[str, Any]], bytes]:
        """The directory's entries, with small files' contents inline."""
        path = self.resolve(rel)
        entries: list[dict[str, Any]] = []
        data = bytearray()
        with os.scandir(path) as it:
            for item in sorted(it, key=lambda e: e.name):
                child = f"{rel}/{item.name}" if rel else item.name
                try:
                    real = self.resolve(child)
                    st = os.stat(real)
                    entry = {"name": item.name, **_stat(st)}
                except (_Refused, OSError):
                    continue  # hidden, outside the mount, special or gone
                room = INLINE_TOTAL - len(data)
                if not entry["is_dir"] and entry["size"] <= min(INLINE_MAX, room):
                    try:
                        with open(real, "rb") as f:
                            contents = f.read(INLINE_MAX + 1)
                    except OSError:
                        contents = None
                    if contents is not None and len(contents) <= min(INLINE_MAX, room):
                        entry["size"] = len(contents)
                        entry["inline"] = True
                        data += contents
                entries.append(entry)
        return entries, bytes(data)

    def read(self, rel: str, offset: int, length: int) -> bytes:
        if offset < 0 or length < 0:
            raise _Refused("io", "negative offset or length")
        path = self.resolve(rel)
        _stat(os.stat(path))  # refuse special files, which could block
        with open(path, "rb") as f:
            f.seek(offset)
            return f.read(min(length, READ_MAX))


def _stat(st: os.stat_result) -> dict[str, Any]:
    if stat_module.S_ISDIR(st.st_mode):
        return {"is_dir": True, "modified": st.st_mtime}
    if not stat_module.S_ISREG(st.st_mode):
        raise _Refused("permission_denied", "not a regular file")
    return {"is_dir": False, "size": st.st_size, "modified": st.st_mtime}


def mounts(volumes: Optional[Sequence[Sequence[Any]]], hidden: Sequence[str]) -> dict[str, Mount]:
    """``volumes`` (pyeryx's ``(host_path, guest_path, read_only)`` tuples)
    by guest path."""
    result: dict[str, Mount] = {}
    for volume in volumes or ():
        if len(volume) not in (3, 4):
            raise ValueError("a volume is (host_path, guest_path, read_only)")
        host_path, guest_path, read_only = volume[0], volume[1], volume[2]
        if not read_only:
            raise ValueError("eryx.run mounts are read-only for now: pass read_only=True")
        if not str(guest_path).startswith(_MOUNT_ROOT):
            raise ValueError(f"volume guest path {guest_path!r} must be under /mnt/")
        if guest_path in result:
            raise ValueError(f"two volumes at {guest_path!r}")
        result[guest_path] = Mount(host_path, guest_path, hidden)
    return result


def answer(mounts: dict[str, Mount], message: dict[str, Any]) -> tuple[dict[str, Any], bytes]:
    """The ``fs_response`` for an ``fs`` message, and the data to send after
    it. Blocking: run it in a thread."""
    reply: dict[str, Any] = {"type": "fs_response", "op_id": message.get("op_id")}
    data = b""
    try:
        mount = mounts.get(message.get("mount", ""))
        if mount is None:
            raise _Refused("not_found", "no such mount")
        rel = message.get("path", "")
        op = message.get("op")
        if op == "stat":
            reply["stat"] = mount.stat(rel)
        elif op == "list":
            reply["entries"], data = mount.list(rel)
        elif op == "read":
            data = mount.read(rel, int(message["offset"]), int(message["len"]))
        else:
            raise _Refused("unsupported", f"unknown operation {op!r}")
    except _Refused as e:
        return {**reply, "error": e.code, "message": str(e)}, b""
    except FileNotFoundError:
        return {**reply, "error": "not_found"}, b""
    except NotADirectoryError:
        return {**reply, "error": "not_a_directory"}, b""
    except IsADirectoryError:
        return {**reply, "error": "is_a_directory"}, b""
    except PermissionError:
        return {**reply, "error": "permission_denied"}, b""
    except Exception as e:  # a bad message too: the code must get an answer
        return {**reply, "error": "io", "message": str(e)}, b""
    reply["data"] = len(data)
    return reply, data
