"""Webcrawler handler — Wylde-side dispatcher for the Webcrawler extension.

The extension_bridge dispatcher imports this module by file path and
calls one of the ``run_*`` functions per the extension manifest's
endpoint mapping (see ``manifest.json``).

What this file does
-------------------
1. Validates each tool's input (URL safety check, etc.) — this used
   to live in the legacy ``_webcrawler_service/tools/*.py`` wrappers
   but has been folded inline since the wrappers were redundant.
2. Forwards the actual HTTP egress through
   :func:`Core.shared.egress_client.forward` so the Gateway's allowlist,
   kill switch, and audit log apply. The pre-refactor scraper used
   ``requests.get`` directly — that's the egress path we replaced.
3. Reuses ``extractor.py`` (pure-python BeautifulSoup, no network)
   for rule-based HTML extraction. It's loaded sibling-to-handler
   via importlib because the dispatcher imports handler.py with a
   synthetic qualified name, so relative imports aren't usable.

The legacy ``ToolInterface`` machinery (ToolMetadata, ToolResult,
ToolError, ToolContext, validate_params) is intentionally not
re-imported here — that layer was the old service-bus contract; under
the bridge each tool is just a function returning a JSON dict, and
the dispatcher wraps errors in :class:`DispatchError`.

Egress path
-----------
``Core.shared.egress_client.forward`` requires a logical destination
key that maps to a Gateway-side allowlist entry; the live Rust
``wylde-gateway`` crate defines those keys. We use ``"web"`` as the
logical key; if the Gateway isn't reachable we catch
:class:`GatewayError` and fall back to a direct ``requests`` call so
the extension stays usable in dev. The fallback is loud (logs
a warning each call) so it can't go unnoticed in production.
"""

from __future__ import annotations

import importlib.util
import ipaddress
import json
import logging
import socket
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional
from urllib.parse import urlparse

logger = logging.getLogger("wylde.extensions.webcrawler.handler")

# ── Lazy load helper modules (extractor) ────────────────────────────────────
#
# handler.py is loaded by the extension_bridge dispatcher via
# ``importlib.util.spec_from_file_location`` under a synthetic
# qualified name (``wylde_extension.Webcrawler.handler``). That parent
# package isn't a real package on sys.path, so ``from .extractor
# import ...`` would fail at runtime. We import sibling helper files
# the same way the dispatcher imports us — file-path based, registered
# under a stable qualified name so re-import is cheap.

_HERE = Path(__file__).resolve().parent


def _load_helper_module(name: str) -> Any:
    """Import a single .py file sibling to this handler."""
    fpath = _HERE / f"{name}.py"
    if not fpath.is_file():
        raise RuntimeError(f"webcrawler: helper file missing: {fpath}")
    qual = f"wylde_extension.webcrawler._helpers.{name}"
    if qual in sys.modules:
        return sys.modules[qual]
    spec = importlib.util.spec_from_file_location(qual, fpath)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"webcrawler: could not import {fpath}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[qual] = module
    spec.loader.exec_module(module)
    return module


# ── URL safety (lifted verbatim from the legacy fetch.py wrapper) ───────────


def _validate_external_url(url: str) -> Optional[str]:
    """Return None if safe to fetch, else a short error string.

    Rejects non-http(s) schemes and any host that resolves to private,
    loopback, link-local, multicast, reserved, metadata, or
    unspecified IP ranges. Same checks as the staged service so we
    don't widen the attack surface during the refactor.
    """
    if not isinstance(url, str) or not url:
        return "URL must be a non-empty string"
    if not url.startswith(("http://", "https://")):
        return "URL must start with http:// or https://"
    try:
        parsed = urlparse(url)
    except Exception:
        return "URL could not be parsed"
    host = parsed.hostname
    if not host:
        return "URL missing hostname"
    try:
        infos = socket.getaddrinfo(host, None)
    except socket.gaierror:
        return "Hostname could not be resolved"
    for info in infos:
        addr = info[4][0]
        try:
            ip = ipaddress.ip_address(addr)
        except ValueError:
            return "Resolved address is not a valid IP"
        if (
            ip.is_private
            or ip.is_loopback
            or ip.is_link_local
            or ip.is_multicast
            or ip.is_reserved
            or ip.is_unspecified
        ):
            return "URL resolves to a disallowed address range"
        if str(ip) == "169.254.169.254":
            return "URL resolves to a metadata endpoint"
    return None


# ── Egress: Gateway-first, requests-fallback ────────────────────────────────


def _gateway_forward(url: str, *, timeout: float) -> Dict[str, Any]:
    """Single GET via Core.shared.egress_client.forward.

    Returns ``{ok, status, content, headers}``. Raises only on
    GatewayBlocked / GatewayDenied so the caller can surface those
    explicitly; transport failures bubble as GatewayError and we
    catch them to fall back to direct requests.
    """
    from Core.shared.egress_client import (  # local import
        GatewayBlocked,
        GatewayDenied,
        GatewayError,
        forward,
    )

    parsed = urlparse(url)
    path = parsed.path or "/"
    if parsed.query:
        path = f"{path}?{parsed.query}"
    try:
        resp = forward(
            dest="web",
            method="GET",
            path=f"//{parsed.netloc}{path}" if parsed.scheme else path,
            body=None,
            headers={"User-Agent": "Wylde-Webcrawler/1.0"},
            timeout=timeout,
        )
    except (GatewayBlocked, GatewayDenied):
        # Policy denial — surface to the caller.
        raise
    except GatewayError:
        # Gateway transport failure (most likely the Gateway server
        # isn't running yet — Phase 5A is still staging). Re-raise
        # for the fallback path.
        raise

    body = resp.body
    if isinstance(body, (bytes, bytearray)):
        try:
            content = body.decode("utf-8", errors="replace")
        except Exception:
            content = str(body)
    elif isinstance(body, dict):
        content = json.dumps(body)
    else:
        content = str(body) if body is not None else ""
    return {
        "ok": resp.ok,
        "status": resp.status,
        "content": content,
        "headers": dict(resp.headers or {}),
    }


def _direct_get(url: str, *, timeout: float) -> Dict[str, Any]:
    """Fallback path used when the Gateway isn't reachable yet.

    TODO(phase-5a): once Gateway/_security_api_merge/ is wired into
    Gateway/ proper this fallback should be removed. Until then,
    direct ``requests.get`` is the only way the Webcrawler stays
    functional in dev.
    """
    import requests  # local import; keeps module import cheap when not in use

    headers = {"User-Agent": "Wylde-Webcrawler/1.0"}
    response = requests.get(url, headers=headers, timeout=timeout)
    return {
        "ok": 200 <= response.status_code < 300,
        "status": response.status_code,
        "content": response.text,
        "headers": dict(response.headers),
    }


def _fetch_via_gateway_or_fallback(url: str, *, timeout: float) -> Dict[str, Any]:
    """Try Gateway first, fall back to direct requests on transport failure.

    Logs at WARNING when the fallback fires so the situation is
    visible in production logs.
    """
    try:
        return _gateway_forward(url, timeout=timeout)
    except Exception as exc:
        # Re-raise GatewayBlocked / GatewayDenied — those are policy,
        # not transport, and must not silently fall back.
        try:
            from Core.shared.egress_client import GatewayBlocked, GatewayDenied

            if isinstance(exc, (GatewayBlocked, GatewayDenied)):
                raise
        except ImportError:
            pass
        logger.warning(
            "webcrawler: Gateway egress failed (%s: %s); falling back to "
            "direct requests. TODO: remove fallback once Gateway "
            "(_security_api_merge) is wired into Gateway/ proper.",
            type(exc).__name__,
            exc,
        )
        return _direct_get(url, timeout=timeout)


# ── Tool handlers ───────────────────────────────────────────────────────────


def run_fetch(params: Dict[str, Any]) -> Dict[str, Any]:
    """Fetch raw URL contents. Mirrors the staged FetchTool.execute."""
    url = params.get("url")
    if not isinstance(url, str) or not url.strip():
        return {
            "status": "error",
            "code": "INVALID_PARAMS",
            "error": "'url' parameter is required and must be a string",
        }
    fmt = str(params.get("format") or "text").lower()
    if fmt not in {"text", "json"}:
        return {
            "status": "error",
            "code": "INVALID_PARAMS",
            "error": f"'format' must be 'text' or 'json' (got {fmt!r})",
        }
    try:
        timeout = float(params.get("timeout", 10))
    except (TypeError, ValueError):
        timeout = 10.0

    err = _validate_external_url(url)
    if err:
        return {"status": "error", "code": "INVALID_URL", "error": err, "url": url}

    try:
        result = _fetch_via_gateway_or_fallback(url, timeout=timeout)
    except Exception as exc:
        return {"status": "error", "code": "FETCH_ERROR", "error": str(exc), "url": url}

    content: Any = result.get("content", "")
    if fmt == "json":
        try:
            content = json.loads(content) if isinstance(content, str) else content
        except json.JSONDecodeError as e:
            return {
                "status": "error",
                "code": "PARSE_ERROR",
                "error": f"failed to parse JSON: {e}",
                "url": url,
            }

    return {
        "status": "ok",
        "url": url,
        "status_code": int(result.get("status", 0)),
        "content": content,
        "format": fmt,
        "content_length": len(str(result.get("content", ""))),
    }


def run_scrape(params: Dict[str, Any]) -> Dict[str, Any]:
    """Scrape HTML + optional CSS selector extraction.

    Single-page only; deep crawling is out of scope for the
    extension surface (run a workflow if you need that).
    """
    url = params.get("url")
    if not isinstance(url, str) or not url.strip():
        return {
            "status": "error",
            "code": "INVALID_PARAMS",
            "error": "'url' parameter is required and must be a string",
        }
    selectors_raw = params.get("selectors") or []
    if not isinstance(selectors_raw, list):
        return {
            "status": "error",
            "code": "INVALID_PARAMS",
            "error": "'selectors' must be a list of CSS selector strings",
        }
    selectors: List[str] = [str(s) for s in selectors_raw]
    try:
        timeout = float(params.get("timeout", 10))
    except (TypeError, ValueError):
        timeout = 10.0

    err = _validate_external_url(url)
    if err:
        return {"status": "error", "code": "INVALID_URL", "error": err, "url": url}

    try:
        fetched = _fetch_via_gateway_or_fallback(url, timeout=timeout)
    except Exception as exc:
        return {
            "status": "error",
            "code": "SCRAPE_ERROR",
            "error": str(exc),
            "url": url,
        }

    html = str(fetched.get("content") or "")
    extracted: Dict[str, Any] = {}
    if selectors:
        try:
            from bs4 import BeautifulSoup  # local import; keeps cold-start cheap

            soup = BeautifulSoup(html, "html.parser")
            for sel in selectors:
                try:
                    elements = soup.select(sel)
                    extracted[sel] = [el.get_text(strip=True) for el in elements]
                except Exception as e:
                    extracted[sel] = {"error": str(e)}
        except ImportError:
            return {
                "status": "error",
                "code": "MISSING_DEPENDENCY",
                "error": "beautifulsoup4 not installed",
                "url": url,
            }

    return {
        "status": "ok",
        "url": url,
        "status_code": int(fetched.get("status", 0)),
        "content": html,
        "extracted": extracted,
        "selectors_used": selectors,
        "content_length": len(html),
    }


def run_extract(params: Dict[str, Any]) -> Dict[str, Any]:
    """Apply extraction rules to HTML, fetching first if only a URL was given."""
    rules = params.get("extraction_rules")
    if not isinstance(rules, dict):
        return {
            "status": "error",
            "code": "INVALID_PARAMS",
            "error": "'extraction_rules' parameter is required and must be an object",
        }
    url = params.get("url")
    html = params.get("html")
    if not isinstance(html, str) and not (isinstance(url, str) and url.strip()):
        return {
            "status": "error",
            "code": "INVALID_PARAMS",
            "error": "either 'url' or 'html' must be provided",
        }

    if not isinstance(html, str) or not html:
        # URL fetch path — go through the gateway-aware helper.
        err = _validate_external_url(url)  # type: ignore[arg-type]
        if err:
            return {"status": "error", "code": "INVALID_URL", "error": err, "url": url}
        try:
            fetched = _fetch_via_gateway_or_fallback(url, timeout=10.0)  # type: ignore[arg-type]
        except Exception as exc:
            return {
                "status": "error",
                "code": "FETCH_ERROR",
                "error": str(exc),
                "url": url,
            }
        html = str(fetched.get("content") or "")

    try:
        extractor_module = _load_helper_module("extractor")
    except Exception as exc:
        return {"status": "error", "code": "EXTRACTOR_LOAD_ERROR", "error": str(exc)}

    try:
        extracted = extractor_module.extractor.extract_by_rules(html, rules)
    except Exception as exc:
        return {"status": "error", "code": "EXTRACTION_ERROR", "error": str(exc)}

    return {
        "status": "ok",
        "url": url,
        "extraction_rules": rules,
        "extracted_data": extracted,
        "fields_extracted": len(extracted) if isinstance(extracted, dict) else 0,
        "html_length": len(html),
    }


__all__ = ["run_fetch", "run_scrape", "run_extract"]
