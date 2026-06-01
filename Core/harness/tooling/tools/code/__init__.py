"""tools/code/ — code-execution tools.

Subprocess-based execution of Python and bash. Outputs are size-capped
(stdout/stderr trimmed) so a runaway program doesn't blow up an LLM
context window. Timeouts are clamped per tool.
"""
