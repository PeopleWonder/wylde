"""Package marker so pytest treats this folder as a package.

Without ``__init__.py`` here, pytest stops walking up at ``tests/`` and
never adds the Wylde repo root to ``sys.path`` — which broke
``test_gateway_integration.py``'s top-level
``from Core.shared.gateway_auth import ...``.
With the marker, pytest walks up past ``device_gate/__init__.py``, sees no
package boundary above it, and prepends the parent (the Wylde root) so
sibling-service imports resolve the same way the running services do.
"""
