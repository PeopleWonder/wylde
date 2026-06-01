"""mDNS advertisement — let phones on the same WiFi find the desktop without
any pairing. They look up `_wylde-link._udp.local` and connect directly to
the gateway with the existing tunnel keys.

Uses the `zeroconf` package which is already in requirements.txt.
"""

import logging
import socket
from typing import Optional

logger = logging.getLogger(__name__)

try:
    from zeroconf import IPVersion, ServiceInfo, Zeroconf

    _ZC_AVAILABLE = True
except ImportError:
    _ZC_AVAILABLE = False


class MdnsAdvertiser:
    def __init__(
        self,
        *,
        hostname: str,
        port: int = 51821,
        service_name: str = "_wylde-link._udp.local.",
        instance_name: str = "Wylde Desktop",
        gateway_port: int = 8021,
        version: str = "1.0",
    ):
        self._hostname = hostname
        self._port = port
        self._service_name = service_name
        self._instance_name = instance_name
        self._gateway_port = gateway_port
        self._version = version
        self._zc: Optional["Zeroconf"] = None
        self._info: Optional["ServiceInfo"] = None

    def start(self) -> bool:
        if not _ZC_AVAILABLE:
            logger.info("mdns: zeroconf not installed — skipping advertisement")
            return False
        try:
            self._zc = Zeroconf(ip_version=IPVersion.V4Only)
            full_name = f"{self._instance_name}.{self._service_name}"
            self._info = ServiceInfo(
                type_=self._service_name,
                name=full_name,
                addresses=[socket.inet_aton(self._local_ip())],
                port=self._port,
                properties={
                    b"gateway": str(self._gateway_port).encode(),
                    b"version": self._version.encode(),
                    b"service": b"wylde-link",
                },
                server=self._hostname + ".",
            )
            self._zc.register_service(self._info)
            logger.info("mdns: registered %s on port %d", full_name, self._port)
            return True
        except Exception as exc:  # noqa: BLE001
            logger.warning("mdns: register failed: %s", exc)
            return False

    def stop(self) -> None:
        if self._zc is not None:
            try:
                if self._info is not None:
                    self._zc.unregister_service(self._info)
                self._zc.close()
            except Exception:  # noqa: BLE001
                pass

    @staticmethod
    def _local_ip() -> str:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            s.connect(("8.8.8.8", 80))
            return str(s.getsockname()[0])
        except OSError:
            return "127.0.0.1"
        finally:
            s.close()
