"""The SDK a komo plugin is written against.

A plugin is a `.py` file under `~/.komo/plugins/` that decorates functions with
`@tool`. The host imports the file, collects what the decorators registered, and
tells komo about it; komo mounts them into its tool catalog, and the model can
call them like any built-in.

    from komo_plugin import tool

    @tool("Return the length of a string.")
    def strlen(text: str) -> int:
        return len(text)

Parameter names and annotations become the JSON schema the model is shown, so
the signature is the contract: annotate the arguments and write a docstring or
pass a description.

A plugin function reaches komo's own tools through `tools`, exactly as a
`run_code` program does — a plugin is a program somebody kept, so it composes
the same way:

    from komo_plugin import tool, tools

    @tool("Count the lines of a file.")
    def linecount(path: str) -> int:
        return len(str(tools.read(path=path)).splitlines())

Everything here is stdlib-only on purpose — a plugin that needs a third-party
package installs it into the interpreter komo runs, and a plugin that needs
none must not have to.
"""

import inspect
import json
import threading
from typing import Any, Callable, Dict, List, Optional

__all__ = ["tool", "tools", "registered_tools", "clear"]

# Name → registration. Ordered by insertion, but komo sorts by name before the
# model ever sees it, so definition order carries no meaning.
_REGISTRY: "Dict[str, _Registration]" = {}


class _Registration:
    def __init__(self, fn: Callable[..., Any], description: str, schema: Dict[str, Any]):
        self.fn = fn
        self.description = description
        self.schema = schema


def tool(description: Optional[str] = None, *, name: Optional[str] = None):
    """Register a function as a tool komo can call.

    `description` is what the model reads to decide whether to call it; the
    docstring is used when it is omitted. `name` overrides the function name.

    A duplicate name raises: two tools answering to one name would make which
    code runs depend on file load order.
    """

    def decorate(fn: Callable[..., Any]) -> Callable[..., Any]:
        tool_name = name or fn.__name__
        text = description or (inspect.getdoc(fn) or "").strip()
        if not text:
            raise ValueError(
                f"tool `{tool_name}` needs a description (pass one, or give the "
                f"function a docstring) — it is what the model reads to decide "
                f"whether to call it"
            )
        if tool_name in _REGISTRY:
            raise ValueError(
                f"tool `{tool_name}` is already registered; two tools cannot "
                f"share a name"
            )
        _REGISTRY[tool_name] = _Registration(fn, text, _schema_of(fn))
        return fn

    # Bare `@tool` (no parentheses) hands the function straight in.
    if callable(description):
        fn, description = description, None
        return decorate(fn)
    return decorate


def registered_tools() -> List[Dict[str, Any]]:
    """The manifest komo is told about: name, description, and JSON schema."""
    return [
        {
            "name": name,
            "description": reg.description,
            "parameters": reg.schema,
        }
        for name, reg in _REGISTRY.items()
    ]


def clear() -> None:
    """Forget every registration — the host calls this before a reload."""
    _REGISTRY.clear()


# The broker for the call running on this thread. Thread-local because the host
# handles every request on its own thread, which is also what lets one plugin
# call block on komo while another runs.
_current = threading.local()


class _Tools:
    """The `tools` object a plugin function calls komo's own tools through.

    Attribute access is resolved per call against the broker the host installed
    for it, so this object holds no state and knows no tool names: a plugin
    reaches whatever the calling turn was offered.

    Outside a call there is nothing to broker, and saying so beats an
    `AttributeError` on a name that is not the problem.
    """

    def _broker(self):
        broker = getattr(_current, "broker", None)
        if broker is None:
            raise RuntimeError(
                "`tools` is only available while komo is calling your @tool "
                "function — it dispatches back into the turn that called you, "
                "so there is nothing to call at import time"
            )
        return broker

    def __getattr__(self, name: str):
        return getattr(self._broker(), name)

    # For a name that is not a legal attribute, mirroring the program-side
    # object a plugin author may already know.
    def __getitem__(self, name: str):
        return getattr(self._broker(), name)


#: See [`_Tools`]. Import it: `from komo_plugin import tools`.
tools = _Tools()


def call(name: str, args: Dict[str, Any], broker: Optional[Any] = None) -> str:
    """Run a registered tool and render its result as the model-facing text.

    `broker` is what `tools` dispatches through for the duration of the call —
    the host passes the same object a program gets.

    A `str` is returned as-is; anything else is JSON-encoded, so a tool can
    return a dict or a list without every plugin re-implementing formatting.
    """
    reg = _REGISTRY.get(name)
    if reg is None:
        raise KeyError(f"no tool named `{name}`")
    previous = getattr(_current, "broker", None)
    _current.broker = broker
    try:
        result = reg.fn(**args)
    finally:
        # Restored rather than cleared: a plugin tool that calls another one
        # through `tools` is nested on this same thread.
        _current.broker = previous
    if inspect.isawaitable(result):
        raise TypeError(
            f"tool `{name}` returned an awaitable; komo plugins are synchronous "
            f"(run your own event loop inside the function if you need one)"
        )
    if isinstance(result, str):
        return result
    if result is None:
        return "(no output)"
    return json.dumps(result, ensure_ascii=False, default=str)


# Annotation → JSON-schema type. Anything else is left untyped rather than
# guessed at: an over-specific schema makes the model's correct call invalid.
_TYPES = {
    str: "string",
    int: "integer",
    float: "number",
    bool: "boolean",
    list: "array",
    dict: "object",
}


def _schema_of(fn: Callable[..., Any]) -> Dict[str, Any]:
    """Derive the model-facing argument schema from the signature.

    A parameter with a default is optional; everything else is required. `*args`
    and `**kwargs` are skipped — the model calls with named arguments, so a
    catch-all it cannot address does not belong in the schema.
    """
    properties: Dict[str, Any] = {}
    required: List[str] = []
    for param in inspect.signature(fn).parameters.values():
        if param.kind in (param.VAR_POSITIONAL, param.VAR_KEYWORD):
            continue
        prop: Dict[str, Any] = {}
        json_type = _TYPES.get(param.annotation)
        if json_type:
            prop["type"] = json_type
        properties[param.name] = prop
        if param.default is inspect.Parameter.empty:
            required.append(param.name)
    return {"type": "object", "properties": properties, "required": required}
