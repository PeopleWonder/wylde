"""Harness developer tooling.

Houses architectural checkers + lint adapters that the LLM (or a human)
can run via the tool catalog.  Lives under ``harness/`` because the
checks encode Wylde-specific contracts the harness itself enforces at
runtime — keeping them near the source of those contracts means they
drift together.

Public surface:

* :mod:`.wylde_check`  — custom architectural rules (no internal HTTP,
                         single manifest write path, dead service refs,
                         etc.).

The off-the-shelf lint wrappers (ruff / eslint / svelte-check / cargo
clippy) live alongside the architectural checker in
``Core/harness/tooling/tools/dev/`` as catalogued tools — separated so
each is independently dispatch-able by id.
"""
