"""tool_search — dynamic discovery over the in-process tool catalog.

Public entrypoint: :func:`run_tool_search`.
"""

from .tool_search import run_tool_search

__all__ = ["run_tool_search"]
