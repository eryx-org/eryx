# Compatibility shim: provides componentize_py_types
#
# These are simple Result types used by componentize_py_async_support.
# Note: Err uses .value (not .error) to match componentize_py's expectations.

from typing import TypeVar, Generic

T = TypeVar('T')
E = TypeVar('E')


class Result(Generic[T, E]):
    """A Result type that can be Ok or Err."""
    pass


class Ok(Result[T, E]):
    """Successful result containing a value."""

    def __init__(self, value: T):
        self.value = value

    def is_ok(self) -> bool:
        return True

    def is_err(self) -> bool:
        return False

    def unwrap(self) -> T:
        return self.value

    def __repr__(self):
        return f"Ok({self.value!r})"


class Err(Result[T, E], BaseException):
    """Error result containing an error value.

    Inherits from BaseException so it can be used in except clauses
    as componentize_py_async_support does.

    Note: Uses .value (not .error) to match componentize_py's expectations.
    """

    def __init__(self, value: E):
        # Use .value for compatibility with componentize_py_async_support
        self.value = value
        BaseException.__init__(self, str(value))

    def is_ok(self) -> bool:
        return False

    def is_err(self) -> bool:
        return True

    def unwrap_err(self) -> E:
        return self.value

    def __repr__(self):
        return f"Err({self.value!r})"
