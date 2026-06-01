"""tools/git/ — git CLI wrappers.

Subprocess shells around the ``git`` binary. Each tool takes a ``path``
parameter (the working tree). Output is JSON-shaped so downstream agents
can parse cleanly without re-scraping porcelain output.
"""
