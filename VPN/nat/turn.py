"""TURN client — request an Allocate from coturn and return the relay
transport address that a mobile peer can use as a fallback path.

This is a short-credential (long-term credential mechanism) TURN allocation.
The request flow is:
  1. Send Allocate (no auth) → server replies 401 with NONCE + REALM.
  2. Re-send Allocate with USERNAME/REALM/NONCE/MESSAGE-INTEGRITY (HMAC-SHA1
     of key = MD5(username:realm:password)).
  3. Server replies with XOR-RELAYED-ADDRESS + XOR-MAPPED-ADDRESS.

For WyldeLink, the allocation is ephemeral, we ask coturn for a relay
address each time a peer requests one and include it in the pair/connect
response.  The peer then keeps the allocation alive with Refresh messages
or re-allocates as needed.

If coturn is not reachable or credentials are wrong, _allocate() returns
None and callers should fall back to advertising the TURN endpoint raw
(the mobile app can do its own allocation).
"""

import hashlib
import hmac
import secrets
import socket
import struct
import logging

logger = logging.getLogger(__name__)

_METHOD_ALLOCATE = 0x003
_CLASS_REQUEST = 0x000
_CLASS_SUCCESS = 0x100
_CLASS_ERROR = 0x110
_MAGIC_COOKIE = 0x2112A442

# Attribute types (RFC 5766 + RFC 5389)
_ATTR_MAPPED_ADDRESS = 0x0001
_ATTR_USERNAME = 0x0006
_ATTR_MESSAGE_INTEGRITY = 0x0008
_ATTR_ERROR_CODE = 0x0009
_ATTR_REALM = 0x0014
_ATTR_NONCE = 0x0015
_ATTR_XOR_MAPPED = 0x0020
_ATTR_REQUESTED_TRANSPORT = 0x0019
_ATTR_LIFETIME = 0x000D
_ATTR_XOR_RELAYED = 0x0016
_ATTR_SOFTWARE = 0x8022


def allocate(
    host: str,
    port: int,
    username: str,
    password: str,
    realm: str = "wylde.local",
    lifetime: int = 600,
    timeout: float = 3.0,
) -> dict | None:
    """Request a relay allocation.  Returns dict with 'relay_ip'/'relay_port'
    and 'mapped_ip'/'mapped_port', or None on failure."""
    if not host or not password:
        return None

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.settimeout(timeout)
        txn = secrets.token_bytes(12)

        # Step 1 — unauthenticated Allocate (get NONCE)
        req = _build_allocate(
            txn, lifetime, username=None, realm=None, nonce=None, key=None
        )
        sock.sendto(req, (host, port))
        data, _ = sock.recvfrom(2048)
        parsed = _parse(data, txn)
        if parsed is None:
            logger.debug("TURN allocate: no parseable response from %s:%d", host, port)
            return None

        if parsed.get("_class") == _CLASS_ERROR:
            nonce = parsed.get("nonce")
            srv_realm = parsed.get("realm") or realm
            if not nonce:
                logger.debug("TURN allocate: 401 without NONCE")
                return None

            # Step 2, authenticated Allocate
            key = _ltc_key(username, srv_realm, password)
            txn2 = secrets.token_bytes(12)
            req2 = _build_allocate(
                txn2, lifetime, username=username, realm=srv_realm, nonce=nonce, key=key
            )
            sock.sendto(req2, (host, port))
            data2, _ = sock.recvfrom(2048)
            parsed2 = _parse(data2, txn2)
            if parsed2 is None or parsed2.get("_class") != _CLASS_SUCCESS:
                logger.debug("TURN allocate: auth failed (%s)", parsed2)
                return None
            parsed = parsed2

        if parsed.get("_class") != _CLASS_SUCCESS:
            return None

        relay = parsed.get("xor_relayed")
        mapped = parsed.get("xor_mapped")
        if not relay:
            return None

        return {
            "relay_ip": relay["ip"],
            "relay_port": relay["port"],
            "mapped_ip": mapped["ip"] if mapped else None,
            "mapped_port": mapped["port"] if mapped else None,
            "lifetime": lifetime,
        }
    except Exception as exc:
        logger.debug("TURN allocate to %s:%d failed: %s", host, port, exc)
        return None
    finally:
        sock.close()


# ── Protocol helpers ──────────────────────────────────────────────────────────


def _ltc_key(username: str, realm: str, password: str) -> bytes:
    return hashlib.md5(f"{username}:{realm}:{password}".encode()).digest()


def _build_allocate(
    txn: bytes,
    lifetime: int,
    username: str | None,
    realm: str | None,
    nonce: bytes | str | None,
    key: bytes | None,
) -> bytes:
    attrs = b""
    # REQUESTED-TRANSPORT = UDP (17)
    attrs += _encode_attr(_ATTR_REQUESTED_TRANSPORT, struct.pack(">BBBB", 17, 0, 0, 0))
    attrs += _encode_attr(_ATTR_LIFETIME, struct.pack(">I", lifetime))

    if (
        username is not None
        and realm is not None
        and nonce is not None
        and key is not None
    ):
        attrs += _encode_attr(_ATTR_USERNAME, username.encode())
        attrs += _encode_attr(_ATTR_REALM, realm.encode())
        attrs += _encode_attr(
            _ATTR_NONCE, nonce if isinstance(nonce, bytes) else nonce.encode()
        )

        # MESSAGE-INTEGRITY placeholder: HMAC is over header+attrs up to (but not
        # including) this attribute.  We compute it with length = current + 24.
        header_len = len(attrs) + 24  # +4 attr header, +20 HMAC-SHA1 value
        header = struct.pack(
            ">HHI12s",
            (_CLASS_REQUEST | _METHOD_ALLOCATE),
            header_len,
            _MAGIC_COOKIE,
            txn,
        )
        mac = hmac.new(key, header + attrs, hashlib.sha1).digest()
        attrs += _encode_attr(_ATTR_MESSAGE_INTEGRITY, mac)

    header = struct.pack(
        ">HHI12s", (_CLASS_REQUEST | _METHOD_ALLOCATE), len(attrs), _MAGIC_COOKIE, txn
    )
    return header + attrs


def _encode_attr(attr_type: int, value: bytes) -> bytes:
    padded = value + b"\x00" * ((-len(value)) % 4)
    return struct.pack(">HH", attr_type, len(value)) + padded


def _parse(data: bytes, txn: bytes) -> dict | None:
    if len(data) < 20:
        return None
    msg_type, msg_len, magic, resp_txn = struct.unpack_from(">HHI12s", data)
    if magic != _MAGIC_COOKIE or resp_txn != txn:
        return None

    cls = msg_type & 0x0110
    out = {"_class": cls}

    offset = 20
    end = 20 + msg_len
    while offset + 4 <= min(end, len(data)):
        attr_type, attr_len = struct.unpack_from(">HH", data, offset)
        offset += 4
        value = data[offset : offset + attr_len]
        offset += attr_len + (-attr_len % 4)

        if attr_type == _ATTR_REALM:
            out["realm"] = value.decode("utf-8", errors="replace")
        elif attr_type == _ATTR_NONCE:
            out["nonce"] = value
        elif attr_type == _ATTR_ERROR_CODE and len(value) >= 4:
            out["error_code"] = (value[2] & 0x07) * 100 + value[3]
        elif attr_type == _ATTR_XOR_RELAYED and len(value) >= 8:
            out["xor_relayed"] = _decode_xor_addr(value)
        elif attr_type == _ATTR_XOR_MAPPED and len(value) >= 8:
            out["xor_mapped"] = _decode_xor_addr(value)

    return out


def _decode_xor_addr(value: bytes) -> dict | None:
    family = value[1]
    if family != 0x01:
        return None
    port = struct.unpack_from(">H", value, 2)[0] ^ (_MAGIC_COOKIE >> 16)
    ip_int = struct.unpack_from(">I", value, 4)[0] ^ _MAGIC_COOKIE
    return {"ip": socket.inet_ntoa(struct.pack(">I", ip_int)), "port": port}
