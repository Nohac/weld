"""Typed conveniences over Weld's restricted BRP endpoint."""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


SCREENSHOT_REQUEST = "weldwm::debug::RemoteScreenshotRequest"
DEBUG_STATUS = "weldwm::debug::RemoteDebugStatus"


class RemoteDebugError(RuntimeError):
    """A transport, JSON-RPC, or Weld automation failure."""


class WeldRemoteClient:
    """Call Weld's BRP methods and wait for frame-aware completion."""

    def __init__(self, url: str, timeout: float = 10.0) -> None:
        if not url.startswith(("http://", "https://")):
            url = f"http://{url}"
        self.url = f"{url.rstrip('/')}/"
        self.timeout = timeout
        self._rpc_id = 0

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        """Send one JSON-RPC request and return its result value."""

        self._rpc_id += 1
        payload: dict[str, Any] = {
            "jsonrpc": "2.0",
            "id": self._rpc_id,
            "method": method,
        }
        if params is not None:
            payload["params"] = params
        request = Request(
            self.url,
            data=json.dumps(payload).encode(),
            headers={"content-type": "application/json"},
            method="POST",
        )
        try:
            with urlopen(request, timeout=self.timeout) as response:
                reply = json.load(response)
        except HTTPError as error:
            raise RemoteDebugError(
                f"BRP returned HTTP {error.code}: {error.reason}"
            ) from error
        except URLError as error:
            raise RemoteDebugError(
                f"cannot reach Weld at {self.url}: {error.reason}"
            ) from error
        except (TimeoutError, json.JSONDecodeError) as error:
            raise RemoteDebugError(
                f"invalid or timed-out BRP response: {error}"
            ) from error

        if error := reply.get("error"):
            raise RemoteDebugError(
                f"BRP {method} failed ({error.get('code', 'unknown')}): "
                f"{error.get('message', error)}"
            )
        return reply.get("result")

    def discover(self) -> Any:
        """Return the methods reachable on this endpoint."""

        return self.call("rpc.discover")

    def status(self) -> dict[str, Any]:
        """Read Weld's reflected remote-debug status resource."""

        result = self.call("world.get_resources", {"resource": DEBUG_STATUS})
        if not isinstance(result, dict) or "value" not in result:
            raise RemoteDebugError("BRP returned an invalid RemoteDebugStatus shape")
        status = result["value"]
        if not isinstance(status, dict):
            raise RemoteDebugError("Weld returned a non-object RemoteDebugStatus")
        return status

    def screenshot(self, path: Path) -> dict[str, Any]:
        """Capture Weld's complete client-plus-shell composition."""

        current = self.status()
        request_id = int(current["last_request_id"]) + 1
        self.call(
            "world.write_message",
            {
                "message": SCREENSHOT_REQUEST,
                "value": {"request_id": request_id, "path": str(path)},
            },
        )

        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            status = self.status()
            if int(status["completed_request_id"]) >= request_id:
                if error := status.get("error"):
                    raise RemoteDebugError(f"Weld screenshot failed: {error}")
                if status.get("ready") and status.get("idle"):
                    return status
            time.sleep(0.01)
        raise RemoteDebugError(f"Weld did not complete request {request_id}")
