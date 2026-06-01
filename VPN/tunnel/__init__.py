"""WireGuard tunnel lifecycle, DNS stub, and tool wrappers.

Split out of the legacy ``models.py`` so each concern lives in its own
file and can be exercised in isolation. The submodules:

* :mod:`wireguard` — wg0/wg1 enable/disable, key generation, peer add/
  remove, transfer counters.
* :mod:`dns_stub` — tiny UDP DNS forwarder that returns SERVFAIL while
  the tunnel is down (leak prevention).
* :mod:`control` — tool-call wrapper functions used by the LLM agent
  loop (status / enable / disable / keygen).
"""
