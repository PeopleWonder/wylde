"""dev/ — developer tooling surface: linters, the architectural
checker, and runtime diagnostics, all exposed as tools.

Six tools live here:

* :mod:`lint_python`       — wraps ``ruff check --output-format=json``
* :mod:`lint_svelte`       — wraps ``svelte-check`` from ``Core/GUI/``
* :mod:`lint_rust`         — wraps ``cargo clippy --message-format=json``
* :mod:`lint_all`          — runs the three above and consolidates
* :mod:`wylde_check`       — re-exposes :mod:`Core.harness.dev.wylde_check`
                             (custom Wylde architectural rules) as a
                             dispatch-able tool
* :mod:`gui_errors_recent` — reads recent Tauri-GUI error events from
                             ``logs/gui_errors.jsonl`` (the sink fed by
                             ``Core/GUI/src/lib/error_sink.ts``)

Each tool returns the standard envelope ``{ok, data, error?}``.  The
four linters plus ``wylde_check`` share a normalised finding shape:
``{rule, severity, file, line, message, context}`` so a single GUI
surface can render results from any source.  ``gui_errors_recent`` is
a diagnostics reader, not a linter — it returns ``{events, count,
total_in_log}`` instead.
"""
