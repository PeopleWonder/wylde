"""device_gate — per-device pairing, tokens, and permission tiers.

Three permission tiers:

* ``read_only`` (default at pairing) — view chat history and the
  conversation feed; cannot invoke tools.
* ``tool_use`` — non-destructive tool calls (read / search / retrieve).
* ``destructive_tool_access`` — full surface, including write / delete /
  execute. The Gateway-side enforcement reads the tool's
  ``requires_confirmation`` flag from its manifest as the "destructive"
  signal; tools missing that flag are treated as non-destructive.

The service exposes a pipe surface (``\\\\.\\pipe\\wylde-device-gate``)
for the GUI and Gateway to drive pairing, token verification, tier
changes, rotation, and revocation. The Gateway calls
:func:`core.verify` on every external request and gates tool calls
against the returned tier.
"""

__all__: list[str] = []
