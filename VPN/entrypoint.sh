#!/bin/sh
# ─── wylde-vpn entrypoint ─────────────────────────────────────────────────────
set -e

PORT="${PORT:-8020}"
VPN_ENABLED="${VPN_ENABLED:-false}"
LINK_ENABLED="${LINK_ENABLED:-false}"
LINK_LISTEN_PORT="${LINK_LISTEN_PORT:-51821}"

log() { printf '[wylde-vpn] %s\n' "$*"; }

# ── WireGuard config generation ───────────────────────────────────────────────
generate_wg0() {
    if [ -z "$VPN_PRIVATE_KEY" ]; then
        log "Generating wg0 key pair (set VPN_PRIVATE_KEY env var to persist)..."
        VPN_PRIVATE_KEY="$(wg genkey)"
        pub="$(printf '%s' "$VPN_PRIVATE_KEY" | wg pubkey)"
        log "  wg0 public key: $pub"
    fi

    cat > /etc/wireguard/wg0.conf <<EOF
[Interface]
PrivateKey = ${VPN_PRIVATE_KEY}
Address = ${VPN_TUNNEL_ADDR:-10.8.0.2/24}
DNS = ${VPN_DNS:-1.1.1.1}
PostUp  = iptables -A OUTPUT -o wg0 -j ACCEPT && touch /run/wylde-vpn-active
PreDown = iptables -D OUTPUT -o wg0 -j ACCEPT && rm -f /run/wylde-vpn-active

[Peer]
PublicKey = ${VPN_PEER_PUBKEY}
AllowedIPs = ${VPN_ALLOWED_IPS:-0.0.0.0/0, ::/0}
Endpoint = ${VPN_ENDPOINT}
PersistentKeepalive = 25
EOF
    chmod 600 /etc/wireguard/wg0.conf
    log "wg0 config written."
}

# ── WyldeLink inbound config ───────────────────────────────────────────────────
generate_wg1() {
    if [ -z "$LINK_PRIVATE_KEY" ]; then
        LINK_PRIVATE_KEY="$(wg genkey)"
        pub="$(printf '%s' "$LINK_PRIVATE_KEY" | wg pubkey)"
        log "  wg1 public key: $pub  (set LINK_PRIVATE_KEY to persist)"
    fi

    cat > /etc/wireguard/wg1.conf <<EOF
[Interface]
PrivateKey = ${LINK_PRIVATE_KEY}
Address = ${LINK_TUNNEL_ADDR:-192.0.2.1/24}
ListenPort = ${LINK_LISTEN_PORT}
EOF
    chmod 600 /etc/wireguard/wg1.conf
    log "wg1 config written."
}

# ── DNS redirect (iptables NAT) ───────────────────────────────────────────────
# Redirect container DNS queries to the stub on port 5300.
# Docker's internal resolver at 127.0.0.11 is left untouched.
setup_dns_redirect() {
    iptables -t nat -A OUTPUT -p udp --dport 53 ! -d 127.0.0.11 \
             -j REDIRECT --to-ports 5300 2>/dev/null || true
    iptables -t nat -A OUTPUT -p tcp --dport 53 ! -d 127.0.0.11 \
             -j REDIRECT --to-ports 5300 2>/dev/null || true
    log "DNS port 53 redirected to stub at port 5300."
}

# ─────────────────────────────────────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────────────────────────────────────

log "Starting (VPN_ENABLED=$VPN_ENABLED  LINK_ENABLED=$LINK_ENABLED)"

# DNS stub runs regardless of VPN state (returns SERVFAIL when VPN is inactive)
python /app/dns_stub.py &
DNS_STUB_PID=$!
log "DNS stub started (pid $DNS_STUB_PID, port 5300)"

if [ "$VPN_ENABLED" = "true" ]; then
    if [ -z "$VPN_PEER_PUBKEY" ] || [ -z "$VPN_ENDPOINT" ]; then
        log "ERROR: VPN_ENABLED=true but VPN_PEER_PUBKEY or VPN_ENDPOINT not set."
        log "  VPN_PEER_PUBKEY — public key of the WireGuard server"
        log "  VPN_ENDPOINT    — host:port of the WireGuard server (e.g. vpn.example.com:51820)"
        exit 1
    fi

    generate_wg0
    setup_dns_redirect

    export WG_QUICK_USERSPACE_IMPLEMENTATION=boringtun
    wg-quick up wg0
    log "wg0 tunnel up — outbound traffic routes through VPN."
else
    log "VPN disabled — passthrough mode (no tunnel)."
fi

if [ "$LINK_ENABLED" = "true" ]; then
    generate_wg1
    export WG_QUICK_USERSPACE_IMPLEMENTATION=boringtun
    wg-quick up wg1
    log "wg1 WyldeLink interface up on UDP port $LINK_LISTEN_PORT."
fi

log "Starting management API on port $PORT..."
exec python /app/wylde_vpn_api.py
