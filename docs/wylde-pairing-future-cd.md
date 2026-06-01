# Wylde Pairing — Future Options C and D

**Captured:** 2026-05-24. **Status:** future-reference, no implementation queued.
**Companion doc:** [`wylde-android-app-plan.md`](wylde-android-app-plan.md) (§5 ships
options A + B as the default pairing UX).

The Android plan ships pairing options **A** (LAN-mDNS first-pair) and **B**
(server-side token persistence) as the default flow. Together they cover the
~95% case: the Wylde user is at home, his phone is on the same wifi as `wylde-vpn`, he
opens the app and sees his server in a discovery list. The two missing cases
both reduce to "I need to pair a device that is *not* on my home LAN" — for
those, this doc keeps two architecturally different fallbacks on the shelf so
the next iteration doesn't start from a blank page.

Neither C nor D has implementation queued. Server-side support for *either*
would need to be designed and built; both add a public-network component A+B
deliberately avoid. This doc is the brief for picking the work back up when
the use case earns its keep.

---

## Table of contents

1. [When you'd reach for C or D](#1-when-youd-reach-for-c-or-d)
2. [Option C — Coordinator-mediated (Headscale or custom)](#2-option-c--coordinator-mediated-headscale-or-custom)
3. [Option D — Cloudflare Tunnel for pairing endpoint only](#3-option-d--cloudflare-tunnel-for-pairing-endpoint-only)
4. [How A+B vs C vs D interact](#4-how-ab-vs-c-vs-d-interact)
5. [Migration path from A+B to A+B+C or A+B+D](#5-migration-path-from-ab-to-abc-or-abd)
6. [Open questions to revisit when picking C or D up](#6-open-questions-to-revisit-when-picking-c-or-d-up)

---

## 1. When you'd reach for C or D

A+B handles the home-LAN case completely. The scenarios below all break that
assumption — the phone is somewhere the LAN-mDNS broadcast can't reach.

- **S1 — Pairing a new device from outside the LAN.** the Wylde user buys a new phone
  while traveling and wants to pair it to his home server *now* without going
  home first. With A+B alone, he has to wait until he's on his home wifi.
- **S2 — Sharing access with another person.** the Wylde user wants to give a friend
  access to a specific service on his Wylde stack (e.g. a shared memory
  workspace, an extension's web UI, a household chat) without exposing the
  pairing endpoint to the open internet.
- **S3 — Sharing across households.** Same as S2 but the other person isn't on
  the Wylde user's LAN at all — household member at a different address, family member
  living elsewhere, etc.
- **S4 — Hostile / untrusted LAN.** the Wylde user's LAN is shared with devices he
  doesn't fully trust (coworking space, AirBnB, hotel) and he doesn't want
  pairing material to traverse it.
- **S5 — LAN with multicast blocked.** Some networks (enterprise wifi,
  guest-VLAN setups) drop mDNS broadcasts. The QR fallback in A+B handles this
  for someone already physically at the server, but doesn't help the
  scenario where the user can't physically reach the QR screen.

S1 is the most likely trigger. S2 / S3 are the second-order "this could be
useful to someone other than the Wylde user" use case. S4 / S5 are edge cases that
don't justify either option on their own.

---

## 2. Option C — Coordinator-mediated (Headscale or custom)

### 2.1 Recommended approach: Headscale

[Headscale](https://github.com/juanfont/headscale) is the open-source
Tailscale-protocol coordinator. It runs on a small VPS, brokers WireGuard key
exchange between nodes, and never sees the data plane — peers talk to each
other directly once the handshake completes. Wylde-vpn's pairing model already
exchanges essentially the same primitives (pubkey, endpoint, allowed IPs); the
Headscale integration shape is a thin shim that registers the phone's pubkey
with the coordinator instead of with `wylde-vpn` directly.

Install effort: a single binary + config + Postgres or SQLite. Hetzner /
DigitalOcean / Fly.io will host it for ~€4/month. Existing Tailscale clients
(or Headscale's own admin CLI) can talk to it for diagnostic / management
purposes.

Integration shape with wylde-vpn:

- `wylde-vpn` gains a "register-via-coordinator" code path alongside its
  existing direct `link.register`. The handshake is the same; only the
  transport differs.
- New action on the Android side: `link.pair-via-coordinator`, which targets
  the coordinator's hostname instead of the local Wylde server.
- The phone still ends up with the same WireGuard config (server pubkey,
  endpoint, peer subnet); the only thing the coordinator changes is *how* the
  phone learned them.

### 2.2 Architecture diagram

```
                       the Wylde user's VPS (Hetzner / DO / etc.)
                       +--------------------------------+
                       | Headscale                      |
                       |   - knows all peers' pubkeys   |
                       |   - brokers handshakes         |
                       |   - never sees data            |
                       +----------------+---------------+
                            ^                    ^
                            |                    |
                            | control plane      | control plane
                            | (key exchange)     | (key exchange)
                            |                    |
                            v                    v
        +-----------------+               +------------------+
        | Phone           |   data plane  | the Wylde user's desktop  |
        | (Wylde Android) |<=============>| (wylde-vpn)      |
        |                 | WireGuard UDP |                  |
        +-----------------+   (direct,    +------------------+
                              after        all Wylde
                              handshake)   services live here
```

The data plane is unchanged from the A+B model — phone talks directly to
desktop over WireGuard once paired. Headscale is only in the picture during
the pairing handshake; after that, it can go down without breaking any
existing tunnel.

### 2.3 Implementation effort

Rough estimate: **1–2 weeks** for the Headscale integration if the Wylde user is OK
running Headscale unmodified. Breakdown:

- Headscale install + DNS + TLS on the VPS: 1 day.
- `wylde-vpn` side: register-via-coordinator handler, ~3–4 days. Reuses the
  existing `link.register` logic; mostly transport plumbing.
- Android side: new "Add server (remote)" entry point that targets the
  coordinator; reuses the existing QR / pairing-code UI from A+B. ~2–3 days.
- Testing + iteration: 2–3 days.

Adds: one VPS to babysit, one more service to update, one more piece of
infrastructure to back up.

### 2.4 Trade-offs

- **Pro:** zero-touch pairing from anywhere — same UX whether the phone is on
  the Wylde user's home LAN or on the other side of the planet.
- **Pro:** scales naturally to multiple users; a coordinator is the right shape
  if Wylde ever has more than one human pairing devices.
- **Pro:** data plane is unchanged — same WireGuard tunnel, same trust model,
  same single auth boundary (Wylde principle #16). Headscale only sees the
  control-plane handshake.
- **Con:** introduces a coordinator service the Wylde user has to run. Another moving
  part to update, secure, monitor.
- **Con:** moderate complexity bump — there are now two pairing paths (direct
  LAN-mDNS and coordinator-mediated) and the code has to pick the right one.
- **Privacy:** Headscale-on-the Wylde user's-own-VPS is the same trust boundary as the
  WireGuard server itself (he owns both). No third party in the loop. The VPS
  provider sees the coordinator's traffic patterns, which is "pairing happened
  at time T between pubkey X and pubkey Y" — substantially less than a typical
  cloud-hosted auth system.

### 2.5 Custom alternative

If Headscale doesn't fit (e.g. the Wylde user wants tighter integration with
`wylde-vpn`'s pairing types, or wants to avoid running Tailscale-compatible
infrastructure), a tiny Rust coordinator service is the alternative. Shape:

- A single Axum HTTP service running on the same VPS.
- Endpoints: `POST /coordinator/announce` (server side checks in with its
  pubkey + endpoint) and `POST /coordinator/pair` (phone side looks up a
  server by short-code or invite token and gets the WireGuard config back).
- Single-tenant or multi-tenant depending on the Wylde user's appetite.

Estimate: **2–3 weeks** beyond what Headscale would have cost, mostly because
this is greenfield code with all the failure modes Headscale has already
solved (key rotation, peer churn, NAT keepalive, etc.). Choose this path only
if the Headscale integration genuinely doesn't fit; otherwise the maintenance
cost dominates.

### 2.6 Decision points (when picking C up)

- Headscale or custom?
- VPS host (Hetzner / DigitalOcean / Fly.io / something the Wylde user already runs)?
- Does coordinator-pairing *replace* direct LAN-mDNS pairing, or coexist?
  (Coexist is the right answer; direct is faster and more private when both
  endpoints are local.)
- Single-tenant or multi-tenant Headscale? (Single-tenant for the Wylde user alone;
  multi-tenant if S2 / S3 are real.)
- How does the phone-side UX choose between direct and coordinator? Auto
  (try direct first, fall back to coordinator) or explicit (user picks)?

---

## 3. Option D — Cloudflare Tunnel for pairing endpoint only

### 3.1 Architecture

[Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/)
gives a self-hosted service a public hostname (with a real TLS cert and
Cloudflare's anycast network in front) without opening any inbound ports on
the home network. Option D applies this *only* to the two pairing endpoints
(`/api/link/pair` and `/api/link/register`), leaving everything else
(Nextcloud, harness, chat, voice, model management) inside the tunnel and
unreachable from the public internet.

```
                    Cloudflare edge
                    +----------------------+
                    | pair.example-domain  |
                    | only routes:         |
                    |   /api/link/pair     |
                    |   /api/link/register |
                    +----------+-----------+
                               |
                               | Cloudflare Tunnel
                               | (outbound from home,
                               |  no inbound port forward)
                               v
        Phone                  +--------------------+
        (Wylde Android) =====> | cloudflared        |
        (HTTPS, public)        |   (running next to |
                               |    wylde-vpn)      |
                               +---------+----------+
                                         |
                                         | localhost
                                         v
                               +--------------------+
                               | wylde-vpn          |
                               |   pairing routes   |
                               |   (others remain   |
                               |    LAN/tunnel only)|
                               +--------------------+
```

Everything except the two pairing routes is reachable only after the
WireGuard tunnel is up — the public surface is intentionally tiny.

### 3.2 Setup

- the Wylde user needs a domain on Cloudflare. If he doesn't have one,
  ~$10/year to register, ~5 minutes to point at Cloudflare DNS.
- Install `cloudflared` on the same box as `wylde-vpn`. Create a named
  tunnel; Cloudflare's CLI walks through the setup. A single config file
  binds the tunnel to a hostname like `pair.example-domain.com`.
- Configure the Wylde Gateway / Caddy to only expose `/api/link/pair` and
  `/api/link/register` on the tunneled hostname. All other routes return 404
  (or are simply not bound). This is a small Caddy / Gateway config change,
  not new code.
- Android side: the pair flow falls back to the tunneled hostname when the
  user picks "Add server (remote)." Reuses the existing pairing-code UI.

### 3.3 Implementation effort

Rough estimate: **3–5 days**.

- Cloudflare account + domain + DNS: ~half a day if the Wylde user already has a
  domain there; a day if not.
- `cloudflared` install + named tunnel + systemd unit: ~half a day.
- Gateway / Caddy route allowlist for the tunneled hostname: ~half a day.
- Android-side change to support a separate pairing hostname: ~1 day. Reuses
  the entry / scan / register code from A+B; only the target URL changes.
- Testing + edge cases (tunnel restart, Cloudflare outage, hostname rotation):
  ~1 day.

No new services to babysit beyond `cloudflared` (which Cloudflare maintains
and updates).

### 3.4 Trade-offs

- **Pro:** minimal public exposure — only two routes (`pair` and `register`),
  not the whole stack. The rest stays VPN-only.
- **Pro:** no new infrastructure beyond a Cloudflare account and a domain.
  Substantially cheaper and lower-maintenance than running a coordinator VPS.
- **Pro:** real Let's Encrypt-equivalent TLS via Cloudflare — solves the
  cert-trust problem for the pairing endpoint without any in-app trust
  anchor work.
- **Pro:** DDoS protection and IP hiding come free.
- **Con:** Cloudflare is in the pairing-handshake loop. They see metadata
  (timestamps, source IP, target hostname) but the payload is end-to-end
  encrypted by the existing token + pubkey scheme. Compromise of Cloudflare
  would let an attacker MITM the handshake; the existing pubkey-pinning
  defense (server pubkey is part of the QR / pairing code, not fetched over
  the wire) limits this to a "fail-closed" outcome — the handshake just
  doesn't complete.
- **Con:** matches the "I accept Cloudflare for the pairing case but not for
  steady-state" trade-off. Different people draw the privacy line in
  different places; this is the option that crosses it the least.
- **Con:** depends on Cloudflare staying free / cheap. If their terms change
  this is the option most at-risk.

### 3.5 Decision points (when picking D up)

- Existing domain on Cloudflare, or register a new one?
- Subdomain for pairing (`pair.example-domain.com`), or main hostname?
- Should the Android app prefer C over D if both are configured? (Probably C,
  since the coordinator is more architecturally pure — but D is simpler to
  deploy.)
- Caddy / Gateway route allowlist — fail closed on any non-pair URL, or
  return a friendly 404? (Fail closed is more defensive.)
- `cloudflared` runs as what user? (Probably its own service account, not the
  Wylde stack's user.)

---

## 4. How A+B vs C vs D interact

|                              | **A+B (default)**                    | **C (coordinator)**           | **D (Cloudflare Tunnel)**     |
|------------------------------|--------------------------------------|-------------------------------|-------------------------------|
| Phone on home LAN?           | Yes — primary case                   | Works but overkill            | Works but unnecessary         |
| Phone elsewhere?             | No — falls back to "wait until home" | Yes                           | Yes                           |
| New infra required?          | None (mDNS already broadcast)        | VPS + Headscale               | Cloudflare account + domain   |
| Server-side work             | Token persistence (~1–2 sessions)    | Coordinator integration (1–2w)| Caddy route allowlist (~1d)   |
| Public attack surface        | None                                 | Coordinator host              | 2 pairing routes              |
| Third party in the loop?     | No                                   | Only if not self-hosted       | Yes (Cloudflare metadata)     |
| Privacy posture              | Best                                 | Best (if self-hosted)         | Moderate                      |
| UX from anywhere             | No                                   | Yes                           | Yes                           |
| Effort to ship               | Smallest                             | Largest                       | Medium                        |
| Mutually exclusive with…?    | Nothing                              | Nothing                       | Nothing                       |

A+B is always on. C and D each address the "first-pair from outside the LAN"
case — they're not mutually exclusive with each other or with A+B. A
deployment could ship A+B and *both* C and D if there were reason; in
practice, pick one of C / D when the scenario justifies it and stick with it.

---

## 5. Migration path from A+B to A+B+C or A+B+D

A+B and C/D both produce the same end state: the phone holds a WireGuard
config it can use to reach the home server. So adding C or D later doesn't
break any existing paired device; it only widens the set of *paths* by which a
new device can get paired.

Concrete steps to add C or D after A+B has shipped:

1. **Server-side: introduce a `pairing-source` enum** on the pairing record
   schema with variants `lan-mdns`, `coordinator`, `tunnel`. Existing records
   default to `lan-mdns`. This is a small migration on the persisted
   `pending-pairings.json` (or whatever the persistence layer settles on) and
   on the live pairing-record cache.
2. **Server-side: add the chosen transport.** For C, that's the
   register-via-coordinator handler. For D, that's the Caddy / Gateway route
   allowlist + `cloudflared` install. Either is additive — existing
   `link.register` over LAN keeps working unchanged.
3. **Android: add the new entry point.** A second "Add server" affordance in
   settings — "Add remote server" — that targets the coordinator or tunneled
   hostname. The existing scan / pairing-code UI is reused; only the target
   changes.
4. **Android: store the pairing source.** Persist alongside the existing
   pairing record so the app can later tell the user "this server was paired
   via coordinator on 2026-09-12" and offer the right re-pair affordance.
5. **(Optional) Auto-detect.** If both transports are configured, the
   pairing UI could try LAN-mDNS first and fall back to coordinator after a
   short timeout. Defer this until it's clear which combination the Wylde user actually
   uses.

Nothing in this migration touches the WireGuard tunnel itself or the
`require_device` bearer flow — both remain identical to the A+B world.

---

## 6. Open questions to revisit when picking C or D up

When the trigger scenario (S1 most likely) finally lands, the punchlist:

- **Which option** — C or D, or both? Default: D first, because the effort is
  smaller and the marginal privacy loss is bounded. Upgrade to C if the
  use case grows beyond the Wylde user's own devices.
- **For C:** Headscale or custom Rust coordinator? Default: Headscale.
- **For C:** Which VPS host? Default: whatever the Wylde user already runs other
  services on, to minimise the new-vendor surface.
- **For D:** Existing Cloudflare account / domain, or new? Default: existing
  if the Wylde user has one; otherwise this is the trigger to register one.
- **For both:** Does the Android app expose a "pairing source" picker, or
  auto-pick? Default: auto-pick, surface in advanced settings only.
- **For both:** What happens when the same phone is paired twice (once via
  LAN-mDNS and once via coordinator/tunnel)? Default: idempotent — server
  recognises the same pubkey and reuses the existing peer record (this
  already works in `wylde-vpn` per `pairing.rs:184`'s `AlreadyRegistered`
  branch). The pairing-source field updates to whichever transport was used
  most recently.
- **For C:** How does the Wylde user rotate or revoke the coordinator's trust
  relationship with `wylde-vpn`? Default: same shape as
  `link.peers.remove` — a CLI affordance on the desktop.
- **Documentation:** when C or D ships, the Android plan's §5, §9, §12, and
  §13 will need a follow-up update to reflect that the "pair from outside the
  LAN" case is now solved. Don't forget to retire this doc's "future" framing
  at that point.
