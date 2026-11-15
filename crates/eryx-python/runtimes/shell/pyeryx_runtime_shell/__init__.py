"""The precompiled eryx shell runtime for pyeryx, for this platform.

pyeryx finds it on its own: `eryx.Sandbox(language="shell")`.
"""

from pathlib import Path

__all__ = ["cwasm_path"]


def cwasm_path() -> str:
    """Path to the precompiled runtime (`runtime-shell.cwasm`)."""
    path = Path(__file__).with_name("runtime-shell.cwasm")
    if not path.is_file():
        raise FileNotFoundError(
            f"{path} is missing: this pyeryx-runtime-shell was built without its runtime"
        )
    return str(path)
