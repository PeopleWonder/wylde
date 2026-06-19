# Gateway egress — SSRF guard

**Status:** SHIPPED (Phase 1 of the WyldeStudy security boundary).
**Crate:** `wylde-gateway` — `src/egress/ssrf.rs`, wired into `src/egress/client.rs`.
**Companion design:** `outputs/wylde-study-security-boundary.md` §2.

## The hole this closes

`egress.forward` (named-pipe + HTTP) lets a declared caller fetch a URL through
an allowlisted destination. The common `web` destination is a **wildcard**
(`url_prefix: "https://"`): the caller supplies the full URL and the only
pre-existing check was *scheme matches + host non-empty* (`validate_path`). That
let any caller reach internal targets:

- loopback — `https://127.0.0.1/…`, `https://localhost/…`, `https://[::1]/…`
- RFC1918 private — `10/8`, `172.16/12`, `192.168/16`
- link-local — `169.254/16`, **including the cloud-metadata endpoint
  `169.254.169.254`**
- IPv6 ULA / link-local — `fc00::/7`, `fe80::/10`
- unspecified / reserved / multicast, and the IPv4-mapped-IPv6 forms of all of
  the above
- **DNS rebinding** — a name that passes a name check, then re-resolves to an
  internal IP at connect time (TOCTOU between validate and `reqwest` connect).

This is a **gateway** vulnerability independent of any one extension — anything
reaching `egress.forward` with a wildcard destination could SSRF.

## The guard

`ssrf::guard_target(url, host_allowlist, allow_private)` runs inside both
`forward()` and `forward_stream()`, **after** `validate_path`/`build_target_url`
and **before** the request is built:

1. **Parse + per-destination allowlist.** Extract the host. If the destination
   declares a non-empty `host_allowlist`, the host must match an entry (exact,
   or suffix via a leading `*.`/`.`). Empty ⇒ any public host.
2. **Block internal names** up front (`localhost`, `*.localhost`, `.local`,
   `.internal`, `.lan`).
3. **Resolve here.** The guard does the DNS lookup itself (`tokio::net::lookup_host`),
   so it owns the time-of-check/time-of-use window. IP-literal hosts flow
   through the same path (resolve to themselves).
4. **Classify every resolved address** against a fail-closed deny-list
   (loopback / private / link-local / ULA / unspecified / broadcast / multicast /
   CGNAT / documentation / reserved, plus IPv4-mapped IPv6 forms). A **single**
   blocked address denies the whole request — no "some addresses are fine".
5. **Pin.** On success the resolved addresses are handed to
   `reqwest::ClientBuilder::resolve_to_addrs(host, addrs)`. `reqwest` then
   connects **only** to those addresses and never re-resolves — closing the
   DNS-rebinding TOCTOU.

A rejection becomes `EgressError::Ssrf(_)`, which folds to the wire code
**`egress_denied`** on both the pipe (`pipe.rs`) and HTTP (`routes/egress.rs`)
surfaces (HTTP `403`), and emits one `blocked / reason: "ssrf"` audit line.

The classification logic mirrors the existing `wylde-ext-webcrawler::ssrf`
guard (Python `_validate_external_url` parity); the gateway adds the
connection **pin** on top, which webcrawler does not have.

## Per-destination knobs (manifest)

Added to the `egress[]` entry schema in `Extensions/<ext>/manifest.json` (and
`<component>/manifest.json`):

```json
{
  "key": "web",
  "url_prefix": "https://",
  "purpose": "Index public web pages.",
  "verify_tls": true,
  "host_allowlist": ["*.wikipedia.org"],   // optional; empty ⇒ any public host
  "allow_private": false                    // optional escape hatch (see below)
}
```

- **`host_allowlist`** — tighten a destination beyond "any public host". Exact
  host, or suffix with a leading `*.` / `.`. Empty (default) keeps back-compat.
- **`allow_private`** — explicit escape hatch for a destination that
  legitimately reaches a private/loopback host (e.g. an internal API). Off by
  default; when on, the deny-list is skipped **for that destination only** (the
  host is still resolved + pinned). **Forced off on wildcard destinations** at
  parse time with a warning — a wildcard + `allow_private` would re-open SSRF to
  the whole internal network for any URL the caller supplies.

## Config

- `WYLDE_EGRESS_SSRF_BLOCK_PRIVATE` — `0`/`false`/`off`/`no` disables the
  deny-list process-wide (fail-open opt-out for trusted single-host
  deployments). **Default: on** (fail-closed). Host allowlist + pinning still
  apply when off.

## Tests

- `egress/ssrf.rs` — classification (loopback, metadata `169.254.169.254`,
  private, CGNAT, reserved, IPv6 ULA/link-local, IPv4-mapped private, public
  allowed), internal-name detection, host-allowlist exact/suffix matching, and
  resolution-level guard tests against IP literals (block loopback / metadata /
  private / localhost; allow public + pin; `allow_private` escape hatch;
  off-allowlist denial).
- `egress/client.rs` — `forward()` end-to-end blocks loopback / metadata /
  private / localhost with `EgressError::Ssrf` (no network needed — the guard
  rejects before connecting).
- `egress/destinations.rs` — manifest parsing of `host_allowlist` /
  `allow_private`, the wildcard `allow_private` guard-rail, and field defaults.

## Not in scope (follow-ups)

- The egress `caller` is still **self-asserted** in the payload (the IPC layer
  doesn't thread the handshake-authenticated caller to handlers). The SSRF guard
  is orthogonal to this — it protects regardless of who the caller claims to be —
  but per-caller *authorization* remains a soft boundary. See boundary doc §5.3.
- WyldeStudy itself still declares no gateway egress destination (`manifest.json`
  with `name: "Wylde_Study"`); adding it is Phase 3 of the boundary work.
