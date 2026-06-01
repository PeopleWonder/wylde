"""DDNS update clients — DuckDNS, No-IP, Cloudflare, Afraid.org.

The pattern: every provider exposes a single HTTPS endpoint that takes the
new IP plus an auth token. The mobile app uses the DDNS hostname (if
configured) instead of the raw IP, so when the home IP rotates the phone
re-resolves and reconnects without any other coordination.
"""

import logging
from typing import Optional

import requests

logger = logging.getLogger(__name__)


def update(
    provider: str,
    *,
    domain: str,
    token: str,
    ip: Optional[str] = None,
    extra: Optional[dict] = None,
) -> dict:
    """Dispatch to the right provider. Returns {ok, message, response?}."""
    provider = (provider or "").lower().strip()
    if provider == "duckdns":
        return _duckdns(domain=domain, token=token, ip=ip)
    if provider == "noip":
        return _noip(domain=domain, token=token, ip=ip)
    if provider == "cloudflare":
        return _cloudflare(domain=domain, token=token, ip=ip, extra=extra or {})
    if provider == "afraid":
        return _afraid(token=token, ip=ip)
    return {"ok": False, "message": f"unknown provider: {provider}"}


def _duckdns(*, domain: str, token: str, ip: Optional[str]) -> dict:
    """https://www.duckdns.org/update?domains=NAME&token=TOKEN&ip=IP"""
    name = domain.split(".")[0]
    params = {"domains": name, "token": token}
    if ip:
        params["ip"] = ip
    try:
        r = requests.get("https://www.duckdns.org/update", params=params, timeout=10)
        ok = r.text.strip() == "OK"
        return {"ok": ok, "message": r.text.strip(), "status": r.status_code}
    except requests.RequestException as exc:
        return {"ok": False, "message": str(exc)}


def _noip(*, domain: str, token: str, ip: Optional[str]) -> dict:
    """No-IP uses HTTP Basic Auth: GET https://USER:PASS@dynupdate.no-ip.com/nic/update?hostname=&myip="""
    user_pass = token  # token is "user:pass"
    if ":" not in user_pass:
        return {"ok": False, "message": 'noip token must be "user:pass"'}
    user, _, password = user_pass.partition(":")
    params = {"hostname": domain}
    if ip:
        params["myip"] = ip
    try:
        r = requests.get(
            "https://dynupdate.no-ip.com/nic/update",
            params=params,
            auth=(user, password),
            timeout=10,
            headers={"User-Agent": "WyldeLink/1.0 wylde@local"},
        )
        text = r.text.strip()
        ok = text.startswith("good ") or text.startswith("nochg ")
        return {"ok": ok, "message": text, "status": r.status_code}
    except requests.RequestException as exc:
        return {"ok": False, "message": str(exc)}


def _cloudflare(*, domain: str, token: str, ip: Optional[str], extra: dict) -> dict:
    """Cloudflare API v4. Requires zone_id + record_id in `extra`.

    Looks up the record if record_id missing and updates it.
    """
    zone_id = extra.get("zone_id", "")
    record_id = extra.get("record_id", "")
    if not zone_id:
        return {"ok": False, "message": "cloudflare requires extra.zone_id"}

    headers = {
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
    }
    base = f"https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records"

    try:
        if not record_id:
            r = requests.get(
                base, params={"name": domain, "type": "A"}, headers=headers, timeout=10
            )
            data = r.json() if r.ok else {}
            results = (data or {}).get("result") or []
            if not results:
                return {"ok": False, "message": f"no A record for {domain}"}
            record_id = results[0]["id"]
            if ip is None:
                ip = results[0].get("content", "")

        if not ip:
            return {"ok": False, "message": "no IP supplied and lookup failed"}

        r = requests.put(
            f"{base}/{record_id}",
            headers=headers,
            timeout=10,
            json={
                "type": "A",
                "name": domain,
                "content": ip,
                "ttl": 60,
                "proxied": False,
            },
        )
        body = r.json() if r.ok else {}
        return {
            "ok": bool(body.get("success")),
            "message": str(body),
            "status": r.status_code,
        }
    except requests.RequestException as exc:
        return {"ok": False, "message": str(exc)}


def _afraid(*, token: str, ip: Optional[str]) -> dict:
    """Afraid.org direct-update URL: https://freedns.afraid.org/dynamic/update.php?TOKEN&IP"""
    url = f"https://freedns.afraid.org/dynamic/update.php?{token}"
    if ip:
        url += f"&{ip}"
    try:
        r = requests.get(url, timeout=10)
        text = r.text.strip()
        ok = "updated" in text.lower() or "no ip change" in text.lower()
        return {"ok": ok, "message": text, "status": r.status_code}
    except requests.RequestException as exc:
        return {"ok": False, "message": str(exc)}
