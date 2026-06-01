# Phase 9 — VPN File-by-File Audit

Audit-only assessment of every file under `Wylde/Device Gate/VPN/_wylde_vpn/`,
filtered through the Wylde user's framing: the bridge exists to talk to the app, the
app does the work. Pure-VPN scope = tunnel + NAT + peers + tunnel health +
the actual proxy. Anything that duplicates app logic gets evicted.

## Summary
- **Total files audited: 53** (47 source files + 6 misc — package inits, JSON state, README, requirements, batch/shell launchers, config.yaml)
- **KEEP: 23** | **DELETE: 6** | **MOVE-TO-GATEWAY: 19** | **MOVE-ELSEWHERE: 5**
- Top finding: **of the 16 route files, 15 belong in Gateway, 1 (`link.py`) belongs in VPN, 0 are dead** — the route layer is almost entirely misplaced app surface area. The VPN today is a Gateway that happens to also run WireGuard.

The four "suspect VPN-internal" route files all turn out to be Gateway concerns:
- `devices.py` → proxies device-gate (LAN approval). VPN doesn't need it.
- `link.py` → wraps wylde-vpn's own `/api/link/*`. The only one Gateway should keep so mobile can self-manage the connection. Could equally live in VPN if Gateway is collapsed; recommended stays in Gateway as a thin proxy.
- `push.py` → calls in-process `push_store`. The store is VPN-internal (peer-keyed), but the *route surface* is mobile UI. Route → Gateway, store stays in VPN.
- `services.py` → reads service manifests + pipe health. Pure dashboard concern → Gateway.

## Per-file evaluation

### Tunnel / NAT / Discovery layer

| File | LOC | What it does | Verdict | Destination |
|---|---|---|---|---|
| `models.py` | 395 | WireGuard wg0/wg1 lifecycle, kill-switch iptables, peer add/remove, `wg show` parsing | **KEEP** | `Device Gate/VPN/tunnel/wireguard.py` (split kill-switch into `tunnel/killswitch.py`) |
| `dns_stub.py` | 106 | Tiny UDP DNS forwarder; SERVFAIL when wg0 down (leak prevention) | **KEEP** | `Device Gate/VPN/tunnel/dns_stub.py` |
| `peer_store.py` | 82 | JSON-backed peer registry with tunnel-IP allocator | **KEEP** | `Device Gate/VPN/peers/store.py` |
| `stun_client.py` | 239 | RFC 5389 binding requests + lite NAT classification + `punch_hole()` | **KEEP** | `Device Gate/VPN/nat/stun.py` (consider merging with `nat/stun_prober.py` — duplicate functionality) |
| `turn_client.py` | 198 | RFC 5766 TURN allocation w/ long-term credentials | **KEEP** | `Device Gate/VPN/nat/turn.py` |
| `nat/__init__.py` | 1 | Package marker | **KEEP** | `Device Gate/VPN/nat/__init__.py` |
| `nat/hole_puncher.py` | 55 | UDP empty-datagram burst to coax NAT mapping | **KEEP** | `Device Gate/VPN/nat/hole_puncher.py` |
| `nat/stun_prober.py` | 238 | Full RFC 5780 four-test NAT classification (richer than `stun_client.classify_nat`) | **KEEP** | `Device Gate/VPN/nat/stun_prober.py` (or merge into `stun.py` — the Wylde user's call) |
| `nat/endpoint_updater.py` | 97 | Daemon thread polls STUN every N s, persists endpoint history, fires `on_change` callback | **KEEP** | `Device Gate/VPN/nat/endpoint_updater.py` |
| `discovery/__init__.py` | 1 | Package marker | **KEEP** | `Device Gate/VPN/discovery/__init__.py` |
| `discovery/mdns_advertiser.py` | 87 | Zeroconf advertise `_wylde-link._udp.local.` | **KEEP** | `Device Gate/VPN/discovery/mdns.py` |
| `discovery/ddns_client.py` | 132 | DuckDNS / NoIP / Cloudflare / Afraid update endpoints | **KEEP** | `Device Gate/VPN/discovery/ddns.py` |
| `tools/wylde_link.py` | 489 | Pairing tokens, peer registration, QR generation, STUN+TURN combination, `_connection_params()`, peer-list w/ handshake refresh, `rehydrate_peers()`, the `_list_services()` dictionary | **KEEP** (split) | `Device Gate/VPN/peers/pairing.py` (tokens, QR, register/connect), `Device Gate/VPN/peers/rehydrate.py`. The `_WYLDE_SERVICES` literal list (lines 438-445) and `_list_services()` should be **DELETED** — the gateway/services route already supersedes it. |
| `tools/vpn_control.py` | 31 | Tool-call wrapper for `models.enable/disable/status/keygen` | **KEEP** | `Device Gate/VPN/tunnel/control.py` (or merge into the Flask route module) |

### Routes (gateway/routes/) — 16 files

| File | LOC | Endpoints exposed | What it does | Verdict | Destination |
|---|---|---|---|---|---|
| `__init__.py` | 1 | — | Package marker | **MOVE-TO-GATEWAY** | `Gateway/routes/__init__.py` |
| `chat.py` | 110 | `/api/chat`, `/api/chat/generate`, `/api/chat/conversations[/<cid>]` | SSE bridge to Ollama + conversation persistence | **MOVE-TO-GATEWAY** | `Gateway/routes/chat.py` — pure app concern |
| `_conversations_store.py` | 108 | (helper) | JSON file at `LINK_DATA_DIR/conversations.json` storing chat history | **MOVE-TO-GATEWAY** | `Gateway/routes/_conversations_store.py` (or better: replace with a pipe call to a real conversations service — file-backed UI state shouldn't live in the network bridge) |
| `conversations.py` | 63 | `/api/conversations[/<cid>]` | Top-level mirror of chat conversations endpoints | **MOVE-TO-GATEWAY** | `Gateway/routes/conversations.py` |
| `devices.py` | 67 | `/api/devices/{pending,approve,deny,approved,<ip>}` | Proxies device-gate service via pipe | **MOVE-TO-GATEWAY** | `Gateway/routes/devices.py` — LAN device-gate is unrelated to VPN peer registry |
| `images.py` | 130 | `/api/images/{generate,library[/<id>],models,loras}` | ComfyUI/image-gen HTTP proxy + base64 library | **MOVE-TO-GATEWAY** | `Gateway/routes/images.py` |
| `link.py` | 77 | `/api/link/{status,peers,peers/remove,stun,pair,qr/<token>}` | Re-proxies wylde-vpn's own link endpoints over the tunnel | **MOVE-TO-GATEWAY** | `Gateway/routes/link.py` — Gateway layer is the place mobile reaches; the actual link logic stays in VPN. (Alternative: kill it and let the mobile client hit `127.0.0.1:8020` directly via tunnel — but Gateway provides auth, so keep the proxy in Gateway.) |
| `models.py` | 114 | `/api/models{,/running,/pull,/generate,/<name>,/registry/{list,search,discovery,swap}}` | Ollama + model-registry proxy | **MOVE-TO-GATEWAY** | `Gateway/routes/models.py` |
| `push.py` | 51 | `/api/push/{subscribe,unsubscribe,pending}` | In-process call to `push_store` | **MOVE-TO-GATEWAY** | `Gateway/routes/push.py` (route stays in Gateway, but it imports `push_store` from VPN — Gateway will need to call into VPN over IPC instead of in-process import) |
| `rag.py` | 46 | `/api/rag/{query,ingest,collections}` | wylde-rag pipe proxy | **MOVE-TO-GATEWAY** | `Gateway/routes/rag.py` |
| `services.py` | 185 | `/api/services{,/<name>/{health,start,stop,wake}}` | Reads `data/manifests/*.json`, classifies status, proxies launcher | **MOVE-TO-GATEWAY** | `Gateway/routes/services.py` |
| `settings.py` | 79 | `/api/settings/{ollama,hardware,hardware/detect}` | Reads/writes `data/settings/ollama.json`, sysmon proxy | **MOVE-TO-GATEWAY** | `Gateway/routes/settings.py` |
| `system.py` | 66 | `/api/system/metrics{,/cpu,/memory,/gpu,/disk}`, `/api/system/{hardware,vram/status}` | wylde-sysmon proxy | **MOVE-TO-GATEWAY** | `Gateway/routes/system.py` |
| `tools.py` | 37 | `/api/tools{,/<id>{,/execute}}` | tool-registry pipe proxy | **MOVE-TO-GATEWAY** | `Gateway/routes/tools.py` |
| `training.py` | 181 | `/api/training/{jobs,datasets,vram-mode,eval,register}` | wylde-trainer pipe proxy | **MOVE-TO-GATEWAY** | `Gateway/routes/training.py` |
| `voice.py` | 69 | `/api/voice/{command,speak,transcribe,health}` | voice-assistant pipe proxy | **MOVE-TO-GATEWAY** | `Gateway/routes/voice.py` |
| `workflows.py` | 358 | `/api/workflows/*` (catalog, run, compose, stream, gates, optimizer, autotuner, n8n bridge) | Massive orchestrator surface | **MOVE-TO-GATEWAY** | `Gateway/routes/workflows.py` |

### Gateway core (gateway/)

| File | LOC | What it does | Verdict | Destination |
|---|---|---|---|---|
| `__init__.py` | 10 | Package docstring only | **MOVE-TO-GATEWAY** | `Gateway/__init__.py` |
| `app.py` | 204 | Flask app factory, route blueprint registration, `start_gateway_thread()` runs it as daemon | **MOVE-TO-GATEWAY** | `Gateway/app.py` — but the launcher logic in `wylde_vpn_api._start_gateway_if_enabled()` should be inverted: Gateway should be its own service, not a side-thread of VPN. |
| `auth.py` | 95 | Bearer token / `X-Peer-Key` peer auth, calls `peer_store.get_peer` | **MOVE-TO-GATEWAY** | `Gateway/auth.py` (will need to call peer_store via IPC, not in-process import) |
| `proxy_core.py` | 163 | `pipe_call`, `http_call`, `ok/error/time_ms` envelope helpers | **MOVE-TO-GATEWAY** | `Gateway/proxy_core.py` — this IS the gateway's reason for existence |
| `rate_limiter.py` | 76 | Per-peer sliding-window rate limit | **MOVE-TO-GATEWAY** | `Gateway/rate_limiter.py` |
| `streaming.py` | 189 | NDJSON-to-SSE bridge + passthrough SSE for orchestrator | **MOVE-TO-GATEWAY** | `Gateway/streaming.py` |

### Proxy / mobile bridge

| File | LOC | What it does | Verdict | Destination |
|---|---|---|---|---|
| `mobile_proxy.py` | 268 | OLD per-route mobile proxy: `_SERVICE_TABLE`, `run_command()` for `inference`/`tool`/`service_status`, async task tracking, calls Ollama/tool-runner directly | **DELETE** | Superseded entirely by `gateway/routes/*` — this is the v1 mobile bridge that the gateway replaced. Confirm by grep'ing references; only `wylde_vpn_api.py` imports it. |
| `wylde_vpn_api.py` | 568 | The Flask management API — `/health`, `/api/vpn/*`, `/api/link/*` (incl. mobile_proxy routes), `/api/restart`, push routes, gateway/discovery startup | **KEEP** (split) | Stays as `Device Gate/VPN/api.py`. Strip out: all `_authed_peer()` + `/api/link/mobile/*` routes (they back `mobile_proxy.py` — dead with it), all `/api/link/push/*` routes (Gateway's `push.py` route covers mobile, push_store stays internal). Keep: `/health`, `/api/vpn/*`, `/api/link/{status,pair,register,stun,peers,connect,services,qr}`, `/api/link/config{GET,PATCH}`, `/api/restart`. The `_start_gateway_if_enabled()` and `_start_discovery_if_enabled()` belong in their own services. |

### Monitoring

| File | LOC | What it does | Verdict | Destination |
|---|---|---|---|---|
| `monitoring/__init__.py` | 1 | Package marker | **KEEP** | `Device Gate/VPN/monitoring/__init__.py` |
| `monitoring/metrics_collector.py` | 65 | Per-peer **gateway** request metrics (count, p50/p99 latency, last-N) | **MOVE-TO-GATEWAY** | `Gateway/metrics.py` — these are HTTP request metrics keyed by gateway peer, not tunnel metrics |
| `monitoring/tunnel_health.py` | 82 | Polls `wg show wg1 latest-handshakes`, marks peers online/stale/offline, fires state-change callback | **KEEP** | `Device Gate/VPN/monitoring/tunnel_health.py` — tunnel-internal |

### Tools / launchers / shared

| File | LOC | What it does | Verdict | Destination |
|---|---|---|---|---|
| `tools/__init__.py` | 144 | TOOLS schema + TOOL_HANDLERS dispatch (Fletch GUI tool surface for VPN/link operations) | **KEEP** | `Device Gate/VPN/tools_manifest.py` (rename so it's not a Python package init that cross-imports from `tools/wylde_link.py`) |
| `consul_client.py` | 327 | Consul service registration + discovery; AUTO-GENERATED from `core/shared/consul_client.py` | **DELETE** | Already a sync copy — the source of truth lives in `Core/shared/`. Make VPN import from there. |
| `ipc.py` | 1400 | Named-pipe transport + HTTP fallback; AUTO-GENERATED from `core/shared/ipc.py` | **DELETE** | Same — sync copy. Import from `Core/shared/`. |
| `manifest.py` | 210 | Service manifest writer + heartbeat; AUTO-GENERATED from `core/shared/manifest.py` | **DELETE** | Same. |
| `errors.py` | 250 | 7-code error taxonomy; AUTO-GENERATED from `core/shared/errors.py` | **DELETE** | Same. |
| `config.py` | 69 | Env-var-driven settings (VPN_*, LINK_*, kill-switch ranges) | **KEEP** | `Device Gate/VPN/config.py` — keep, but split kill-switch ranges into `tunnel/killswitch.py` if you want clean modules |
| `config.yaml` | 70 | YAML defaults for every config knob | **KEEP** (split) | `Device Gate/VPN/config.yaml` — strip the `gateway:` block (moves with Gateway), strip `consul:` if Consul is being phased out |
| `requirements.txt` | 11 | Flask, qrcode, pyyaml, zeroconf, msgpack, pywin32, requests, sseclient-py, dnspython | **KEEP** (trim) | `Device Gate/VPN/requirements.txt` — drop `sseclient-py` (only Gateway streaming uses it), `dnspython` (unused — grep confirms). Add comments showing which dep belongs to which layer. |
| `run.py` | 125 | YAML→env, manifest write, heartbeat, then `wylde_vpn_api.main()` | **KEEP** | `Device Gate/VPN/run.py` |
| `startup.py` | 56 | Windows Startup folder install/uninstall | **MOVE-ELSEWHERE** | `Core/installer/startup_windows.py` — generic launcher concern, every Wylde service needs the same shape |
| `start_wylde_vpn.bat` | 53 | Windows venv setup + run.py | **KEEP** | `Device Gate/VPN/start_wylde_vpn.bat` |
| `entrypoint.sh` | 142 | Linux/Docker entry: kill-switch, generate wg0/wg1 conf, wg-quick up, exec api | **KEEP** | `Device Gate/VPN/entrypoint.sh` — this is the Linux/Docker path; native Windows uses run.py. Both survive. (See Install/download story below.) |
| `README.md` | 8 | One-paragraph blurb | **KEEP** | `Device Gate/VPN/README.md` (will need rewrite once layout settles) |
| `data/wylde-link/endpoint-history.json` | 502 | Runtime data — recent ext endpoints from `endpoint_updater` | **KEEP** | Stays under `data/` — gitignore'd runtime state, not source. |

## KEEP files — dependency map

External deps shorthand: `core-shared` = `Core/shared/{ipc,consul_client,manifest,errors}.py` (auto-synced today). `gateway-client` = the new pipe/HTTP client to call into Gateway.

- **`config.py`** — depends on: stdlib only. external: env vars only.
- **`config.yaml`** — depends on: nothing. external: read by `run.py`, `wylde_vpn_api.py`, `gateway/app.py`.
- **`models.py`** — depends on: `config.py`. external: `wg`, `wg-quick`, `iptables`, `ip` binaries (Linux only).
- **`dns_stub.py`** — depends on: stdlib + env vars. external: `/run/wylde-vpn-active` marker (touched by `models._mark_active`).
- **`peer_store.py`** — depends on: `config.py`. external: `LINK_DATA_DIR/peers.json`.
- **`stun_client.py`** — depends on: stdlib only.
- **`turn_client.py`** — depends on: stdlib only.
- **`nat/__init__.py`** — empty.
- **`nat/hole_puncher.py`** — depends on: stdlib only.
- **`nat/stun_prober.py`** — depends on: stdlib only. (Note: largely overlaps with `stun_client.classify_nat`.)
- **`nat/endpoint_updater.py`** — depends on: `config.py`, `nat/stun_prober.py`. external: `LINK_DATA_DIR/endpoint-history.json`. Calls a user-supplied `on_change` (today: `wylde_vpn_api._endpoint_change_callback` → `push_store.broadcast`).
- **`discovery/__init__.py`** — empty.
- **`discovery/mdns_advertiser.py`** — depends on: `zeroconf` (optional).
- **`discovery/ddns_client.py`** — depends on: `requests`.
- **`monitoring/__init__.py`** — empty.
- **`monitoring/tunnel_health.py`** — depends on: `models.py`. Calls a user-supplied `on_state_change` (likely fires push notifications).
- **`push_store.py`** — depends on: `config.py`. external: `LINK_DATA_DIR/push.json`. Webhook delivery via `urllib`.
- **`tools/wylde_link.py`** — depends on: `config.py`, `models.py`, `peer_store.py`, `stun_client.py`, `turn_client.py`. external: `qrcode` (optional), Flask `jsonify/make_response`.
- **`tools/vpn_control.py`** — depends on: `models.py`.
- **`tools/__init__.py`** (TOOLS manifest) — depends on: `tools/vpn_control.py`, `tools/wylde_link.py`.
- **`wylde_vpn_api.py`** (after gut) — depends on: `config.py`, `models.py`, `peer_store.py`, `push_store.py`, `tools/wylde_link.py`, core-shared `ipc`, core-shared `consul_client`, core-shared `manifest`. external: starts Gateway service, starts mDNS + endpoint-updater.
- **`run.py`** — depends on: `config.yaml`, `wylde_vpn_api.py`, core-shared `manifest`.
- **`tools_manifest.py`** (renamed from `tools/__init__.py`) — depends on: handlers in `tools/`.
- **`entrypoint.sh`** — Linux-only. Same env-var contract as `run.py`.
- **`start_wylde_vpn.bat`** — Calls `run.py` after venv bootstrap.

## Proposed final layout for `Wylde/Device Gate/VPN/`

```
Device Gate/VPN/
├── README.md
├── requirements.txt
├── config.yaml                    # (gateway: block removed)
├── config.py                      # env knobs
├── run.py                         # native Windows launcher
├── start_wylde_vpn.bat
├── entrypoint.sh                  # Linux/Docker launcher (still wanted)
├── api.py                         # ex-wylde_vpn_api.py, gutted: only VPN/link/restart routes
├── tools_manifest.py              # ex-tools/__init__.py — TOOLS / TOOL_HANDLERS
├── tunnel/
│   ├── __init__.py
│   ├── wireguard.py               # ex-models.py wg-quick + key gen
│   ├── killswitch.py              # ex-models.py iptables OUTPUT policy
│   ├── dns_stub.py
│   └── control.py                 # ex-tools/vpn_control.py (status/enable/disable/keygen tool fns)
├── nat/
│   ├── __init__.py
│   ├── stun.py                    # consolidated stun_client.py + nat/stun_prober.py
│   ├── turn.py                    # ex-turn_client.py
│   ├── hole_puncher.py
│   └── endpoint_updater.py
├── discovery/
│   ├── __init__.py
│   ├── mdns.py                    # ex-mdns_advertiser.py
│   └── ddns.py                    # ex-ddns_client.py
├── peers/
│   ├── __init__.py
│   ├── store.py                   # ex-peer_store.py
│   ├── pairing.py                 # ex-tools/wylde_link.py — token issuance, QR, register/connect, _connection_params
│   ├── rehydrate.py               # ex-tools/wylde_link.rehydrate_peers
│   └── push.py                    # ex-push_store.py — peer-keyed notification queue
├── monitoring/
│   ├── __init__.py
│   └── tunnel_health.py           # wg1 handshake polling + state changes
└── data/                          # gitignored runtime state
    └── wylde-link/
        ├── peers.json
        ├── push.json
        ├── conversations.json     # (deletes when chat moves to Gateway)
        └── endpoint-history.json
```

What leaves:
- All 16 `gateway/routes/*` files → `Wylde/Gateway/routes/`
- All `gateway/{app,auth,proxy_core,rate_limiter,streaming}.py` → `Wylde/Gateway/`
- `monitoring/metrics_collector.py` → `Wylde/Gateway/metrics.py`
- `mobile_proxy.py` → deleted (gateway routes already cover it)
- `consul_client.py`, `ipc.py`, `manifest.py`, `errors.py` → deleted (sync from `Core/shared/` already)
- `startup.py` → `Core/installer/startup_windows.py`

## Install / download story

Native binaries needed at runtime:
- **Linux/Docker path** (`entrypoint.sh`): `wg`, `wg-quick`, `iptables`, `boringtun` (userspace WireGuard). Today these live in the container image.
- **Windows path** (`run.py`): None of the above run natively — only the Flask management API serves. Tunnel-control endpoints return errors. If we want native WG on Windows we need `wireguard.exe` + the WG service, or we lean on `boringtun` (Rust binary). Today: nothing is downloaded; the Windows path is essentially a "remote control" stub for the Linux tunnel.

Proposed `download_vpn.py` shape (for native Windows):
```python
# Wylde/Device Gate/VPN/download_vpn.py
# Run once at install time; idempotent.

ASSETS = {
    "boringtun": {
        "url": "https://github.com/cloudflare/boringtun/releases/download/.../boringtun-cli-x86_64-pc-windows-msvc.exe",
        "sha256": "...",
        "dest": "bin/boringtun.exe",
    },
    "wg-tools": {
        "url": "https://download.wireguard.com/windows-client/wireguard-x64.msi",
        "sha256": "...",
        "dest": "bin/wireguard.msi",
        "post_install": "msiexec /i bin/wireguard.msi /qn",
    },
}
# Verify SHA256 before extracting, print progress, support --check (verify only).
```

`entrypoint.sh` (Linux) — **survives**. It's the Docker/WSL2 path and does work that Windows can't (iptables OUTPUT DROP, wg-quick up, NAT redirect for DNS). It and `run.py` should both stay; they target different hosts. The duplication between them (env-var contract, config gen) is acceptable because the Linux path needs root + iptables and the Windows path doesn't.

## Open questions for the Wylde user

1. **`stun_client.py` vs `nat/stun_prober.py`** — substantial overlap (both do STUN binding, both do classification, the prober is RFC-5780-correct, the client is "lite"). Merge into one `nat/stun.py`, or keep both for the discovery path that just wants a public IP fast?

2. **`gateway/routes/link.py`** — wraps wylde-vpn's own `/api/link/*` so the mobile client can call them through the tunnel with auth. If Gateway moves out of VPN entirely, this re-proxy becomes a Gateway→VPN call. Two options:
   - Gateway re-proxies (current shape) — small overhead, clean auth boundary.
   - Mobile client hits `127.0.0.1:8020` directly via tunnel — no auth (pre-Gateway state). 
   
   Recommend keeping the Gateway proxy.

3. **`push_store.py` location** — peer-keyed notification queue. Gateway's `push.py` route imports it directly today. If Gateway moves to a separate service, it needs IPC into VPN. Alternative: lift push_store into Gateway entirely (it's tied to peers but the data is "messages for the mobile app", not tunnel state). Recommend keeping it in VPN under `peers/push.py`, exposing through pipe.

4. **`_conversations_store.py`** — file-backed chat history under `LINK_DATA_DIR`. Should chat history live anywhere near the VPN? Probably belongs in a Conversations service that Gateway proxies into. Today it's a local JSON file co-tenanted with the network bridge — historical accident. Move with the route to Gateway as a stopgap; replace with a real service later.

5. **Consul** — `consul_client.py` is auto-synced from `Core/shared`. If Consul is being phased out (see manifests-based discovery in `services.py`), the entire Consul registration block in `wylde_vpn_api.main()` is dead code.

6. **`tools/__init__.py` (TOOLS schema)** — does anything still consume this? Grep for `from tools import TOOLS` / `TOOL_HANDLERS` to confirm. If only the legacy tool-runner consumed it and that's gone, this can be **DELETE** rather than KEEP.

7. **`mobile_proxy.py` confirmation** — verdict assumes nothing imports it except `wylde_vpn_api.py` and the gateway has fully replaced it. Worth a final grep before deletion.

8. **`config.yaml` `consul:` and `gateway:` blocks** — confirm the Wylde user is OK stripping them when the audit is executed; the runtime defaults in `run.py` cover the absence.
