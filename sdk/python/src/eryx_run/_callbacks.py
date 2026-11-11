"""Callbacks: host functions sandboxed code can ``await``, as in pyeryx."""

from __future__ import annotations

import asyncio
import inspect
from collections.abc import Iterator, Sequence
from typing import Any, Callable, Optional, TypedDict, TypeVar, Union

F = TypeVar("F", bound=Callable[..., Any])


class CallbackDict(TypedDict, total=False):
    """Dictionary format for defining callbacks, as in pyeryx."""

    name: str
    """The callback name (required)."""

    fn: Callable[..., Any]
    """The callback function, sync or async (required)."""

    description: str
    """Optional description of what the callback does."""

    schema: dict[str, Any]
    """Optional JSON Schema for parameters. The hosted API doesn't use it yet:
    arguments are passed through as the code gave them."""


class CallbackRegistry:
    """A collection of callbacks, built with a decorator or ``add``.

    Example:
        callbacks = CallbackRegistry()

        @callbacks.callback(description="Look up a user by id")
        async def get_user(id: int) -> dict:
            ...

        sandbox = Sandbox(callbacks=callbacks)
    """

    def __init__(self) -> None:
        self._callbacks: list[CallbackDict] = []

    def callback(
        self,
        name: Optional[str] = None,
        description: str = "",
        schema: Optional[dict[str, Any]] = None,
    ) -> Callable[[F], F]:
        """Register the decorated function, named after it unless ``name`` is
        given."""

        def register(fn: F) -> F:
            self.add(fn, name=name, description=description, schema=schema)
            return fn

        return register

    def add(
        self,
        fn: Callable[..., Any],
        name: Optional[str] = None,
        description: str = "",
        schema: Optional[dict[str, Any]] = None,
    ) -> None:
        """Register ``fn``, named after it unless ``name`` is given."""
        entry: CallbackDict = {
            "name": name or fn.__name__,
            "fn": fn,
            "description": description or inspect.getdoc(fn) or "",
        }
        if schema is not None:
            entry["schema"] = schema
        self._callbacks.append(entry)

    def __len__(self) -> int:
        return len(self._callbacks)

    def __iter__(self) -> Iterator[CallbackDict]:
        return iter(self._callbacks)


Callbacks = Union[CallbackRegistry, Sequence[CallbackDict]]


def normalize(callbacks: Optional[Callbacks]) -> dict[str, CallbackDict]:
    """The callbacks by name, checking each has a name and a function."""
    by_name: dict[str, CallbackDict] = {}
    for callback in callbacks or ():
        name, fn = callback.get("name"), callback.get("fn")
        if not name or not callable(fn):
            raise ValueError("each callback needs a 'name' and a callable 'fn'")
        if name in by_name:
            raise ValueError(f"callback {name!r} is defined twice")
        by_name[name] = callback
    return by_name


async def invoke(callback: CallbackDict, args: dict[str, Any]) -> Any:
    """Call ``callback`` with the code's keyword arguments. A sync function
    runs in a thread, so a slow one doesn't hold up the connection."""
    fn = callback["fn"]
    if inspect.iscoroutinefunction(fn):
        return await fn(**args)
    result = await asyncio.to_thread(fn, **args)
    if inspect.isawaitable(result):
        result = await result
    return result
