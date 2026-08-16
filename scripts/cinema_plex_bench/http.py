"""Small, dependency-free HTTP helpers with credential-safe diagnostics."""

from __future__ import annotations

import json
import re
import time
from typing import Any
import urllib.error
import urllib.request
from urllib.parse import urlsplit, urlunsplit


class HttpFailure(RuntimeError):
    def __init__(self, method: str, url: str, status: int | None, detail: str):
        self.method = method
        self.status = status
        self.safe_url = safe_url(url)
        super().__init__(f"{method} {self.safe_url} -> {status or 'network error'}: {detail}")


def safe_url(url: str) -> str:
    parsed = urlsplit(url)
    host = parsed.hostname or ""
    if parsed.port:
        host = f"{host}:{parsed.port}"
    return urlunsplit((parsed.scheme, host, parsed.path, "", ""))


class HttpClient:
    def __init__(self, timeout_seconds: float):
        self.timeout_seconds = timeout_seconds

    def request(
        self,
        url: str,
        *,
        method: str = "GET",
        headers: dict[str, str] | None = None,
        json_body: dict[str, Any] | None = None,
        timeout_seconds: float | None = None,
    ) -> tuple[bytes, str, dict[str, str]]:
        data = None
        request_headers = dict(headers or {})
        if json_body is not None:
            data = json.dumps(json_body, separators=(",", ":")).encode()
            request_headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=data, method=method, headers=request_headers)
        timeout = timeout_seconds or self.timeout_seconds
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return (
                    response.read(),
                    response.geturl(),
                    {key.lower(): value for key, value in response.headers.items()},
                )
        except urllib.error.HTTPError as error:
            detail = error.read(512).decode(errors="replace").strip().replace("\n", " ")
            raise HttpFailure(method, url, error.code, detail or error.reason) from error
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            reason = getattr(error, "reason", error)
            raise HttpFailure(method, url, None, str(reason)) from error

    def json(self, url: str, **kwargs: Any) -> Any:
        payload, _, _ = self.request(url, **kwargs)
        try:
            return json.loads(payload or b"null")
        except json.JSONDecodeError as error:
            raise HttpFailure(kwargs.get("method", "GET"), url, None, "response was not JSON") from error

    def first_bytes(self, url: str, headers: dict[str, str], count: int = 64 * 1024) -> int:
        request_headers = dict(headers)
        request_headers.setdefault("Range", f"bytes=0-{count - 1}")
        request = urllib.request.Request(url, headers=request_headers)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout_seconds) as response:
                data = response.read(count)
        except urllib.error.HTTPError as error:
            detail = error.read(512).decode(errors="replace").strip().replace("\n", " ")
            raise HttpFailure("GET", url, error.code, detail or error.reason) from error
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise HttpFailure("GET", url, None, str(getattr(error, "reason", error))) from error
        if not data:
            raise HttpFailure("GET", url, None, "empty media response")
        return len(data)


def _playlist_uri(text: str) -> tuple[str | None, bool]:
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    master = any(line.startswith("#EXT-X-STREAM-INF") for line in lines)
    if master:
        for index, line in enumerate(lines[:-1]):
            if line.startswith("#EXT-X-STREAM-INF"):
                for candidate in lines[index + 1 :]:
                    if not candidate.startswith("#"):
                        return candidate, True
    pending_media = False
    for line in lines:
        if line.startswith("#EXTINF"):
            pending_media = True
        elif pending_media and not line.startswith("#"):
            return line, False
    return None, master


def _master_attributes(text: str) -> dict[str, Any]:
    for line in text.splitlines():
        if not line.startswith("#EXT-X-STREAM-INF:"):
            continue
        attributes = {
            match.group(1): match.group(2).strip('"')
            for match in re.finditer(r'([A-Z0-9-]+)=("[^"]*"|[^,]*)', line)
        }
        bandwidth = attributes.get("AVERAGE-BANDWIDTH") or attributes.get("BANDWIDTH")
        try:
            bitrate = int(bandwidth) / 1000 if bandwidth is not None else None
        except ValueError:
            bitrate = None
        return {
            "advertised_output_bitrate_kbps": bitrate,
            "advertised_output_codecs": attributes.get("CODECS"),
            "advertised_output_resolution": attributes.get("RESOLUTION"),
        }
    return {}


def wait_hls_ready(
    http: HttpClient,
    url: str,
    headers: dict[str, str],
    timeout_seconds: float,
) -> dict[str, Any]:
    """Wait for a media playlist and read its first completed segment."""
    from urllib.parse import urljoin

    deadline = time.monotonic() + timeout_seconds
    current = url
    playlist_reads = 0
    advertised: dict[str, Any] = {}
    while time.monotonic() < deadline:
        try:
            payload, final_url, _ = http.request(
                current,
                headers=headers,
                timeout_seconds=max(0.1, min(http.timeout_seconds, deadline - time.monotonic())),
            )
            playlist_reads += 1
            text = payload.decode("utf-8", "replace")
            uri, master = _playlist_uri(text)
            if master and uri:
                advertised.update(_master_attributes(text))
                current = urljoin(final_url, uri)
                continue
            if uri:
                segment_url = urljoin(final_url, uri)
                size = http.first_bytes(segment_url, headers)
                return {
                    "playlist_reads": playlist_reads,
                    "first_segment_bytes": size,
                    **advertised,
                }
        except HttpFailure as error:
            if error.status not in (404, 409, 425, 429, 503):
                raise
        time.sleep(0.1)
    raise HttpFailure("GET", current, None, f"no completed HLS segment within {timeout_seconds:.1f}s")
