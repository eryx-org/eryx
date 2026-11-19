"""The precompiled eryx JavaScript runtime for pyeryx, for this platform.

pyeryx finds it on its own: `eryx.Sandbox(language="javascript")`.
"""

from pathlib import Path

__all__ = ["cwasm_path"]


def cwasm_path() -> str:
    """Path to the precompiled runtime (`runtime-javascript.cwasm`)."""
    path = Path(__file__).with_name("runtime-javascript.cwasm")
    if not path.is_file():
        raise FileNotFoundError(
            f"{path} is missing: this pyeryx-runtime-javascript was built without its runtime"
        )
    return str(path)
