"""The precompiled eryx shell runtime for pyeryx, for this platform.

pyeryx finds it on its own: `eryx.Sandbox(language="shell")`. It also holds the
shell's default external commands, uutils coreutils (`coreutils.cwasm`).
"""

from pathlib import Path

__all__ = ["coreutils_path", "cwasm_path"]


def cwasm_path() -> str:
    """Path to the precompiled runtime (`runtime-shell.cwasm`)."""
    path = Path(__file__).with_name("runtime-shell.cwasm")
    if not path.is_file():
        raise FileNotFoundError(
            f"{path} is missing: this pyeryx-runtime-shell was built without its runtime"
        )
    return str(path)


def coreutils_path() -> str:
    """Path to the precompiled uutils coreutils (`coreutils.cwasm`)."""
    path = Path(__file__).with_name("coreutils.cwasm")
    if not path.is_file():
        raise FileNotFoundError(
            f"{path} is missing: this pyeryx-runtime-shell was built without coreutils"
        )
    return str(path)
