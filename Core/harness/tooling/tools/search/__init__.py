"""tools/search/ — code-search tools.

Ripgrep wrappers with a Python regex/glob fallback for hosts that don't
ship `rg`. The fallback skips the usual noise dirs (.git, node_modules,
venv, __pycache__, dist, build) so it stays usable on real trees.
"""
