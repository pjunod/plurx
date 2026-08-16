"""Cinema/plurx and Plex server adapters behind one benchmark contract."""

from __future__ import annotations

from dataclasses import dataclass, field
import os
from typing import Any
import urllib.parse
import uuid
import xml.etree.ElementTree as ET

from .config import ConfigError
from .http import HttpClient, wait_hls_ready


@dataclass
class StreamHandle:
    url: str
    headers: dict[str, str]
    session_id: str | None
    client_seek_seconds: float
    playback_mode: str
    output_video_codec: str | None
    output_audio_codec: str | None
    output_bitrate_kbps: int | None
    details: dict[str, Any] = field(default_factory=dict)


class ServerRunner:
    product = "unknown"

    def __init__(
        self,
        key: str,
        config: dict[str, Any],
        token: str,
        http: HttpClient,
    ):
        self.key = key
        self.config = config
        self.token = token
        self.base = config["base_url"]
        self.http = http
        self._identity: dict[str, Any] | None = None

    @property
    def auth_headers(self) -> dict[str, str]:
        raise NotImplementedError

    def identity(self) -> dict[str, Any]:
        raise NotImplementedError

    def media_info(self, medium: dict[str, Any]) -> dict[str, Any]:
        raise NotImplementedError

    def prepare_stream(
        self,
        medium: dict[str, Any],
        scenario: dict[str, Any],
        start_seconds: float,
        trial_id: str,
    ) -> StreamHandle:
        raise NotImplementedError

    def wait_ready(self, stream: StreamHandle, timeout_seconds: float) -> dict[str, Any]:
        if stream.playback_mode == "direct_play":
            return {"first_media_bytes": self.http.first_bytes(stream.url, stream.headers)}
        return wait_hls_ready(self.http, stream.url, stream.headers, timeout_seconds)

    def close_stream(self, stream: StreamHandle) -> None:
        del stream


class CinemaRunner(ServerRunner):
    product = "cinema/plurx"

    @property
    def auth_headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.token}"}

    def api_url(self, path: str) -> str:
        return f"{self.base}/api/v1{path}"

    def identity(self) -> dict[str, Any]:
        if self._identity is None:
            response = self.http.json(self.api_url("/server"))
            self._identity = {
                "server": self.key,
                "server_name": response.get("name"),
                "server_product": self.product,
                "server_version": response.get("version"),
                "server_commit": response.get("build") or self.config.get("commit"),
                "server_instance_id": response.get("instance_id"),
            }
        return dict(self._identity)

    def _binding(self, medium: dict[str, Any]) -> dict[str, Any]:
        return medium["bindings"][self.key]

    def _decision(self, medium: dict[str, Any]) -> dict[str, Any]:
        file_id = self._binding(medium)["file_id"]
        query = urllib.parse.urlencode(
            {
                "client": "cinema-plex-benchmark",
                "device": "common-ffmpeg-decoder",
                "vcodec": "h264,hevc,vp9,av1",
                "acodec": "aac,ac3,eac3,opus,mp3,flac",
                "container": "mp4,mov,webm,mkv",
                "maxheight": 4320,
                "hdr": 1,
                "dv": 1,
                "force": "original",
            }
        )
        return self.http.json(
            self.api_url(f"/files/{file_id}/decision?{query}"), headers=self.auth_headers
        )

    def media_info(self, medium: dict[str, Any]) -> dict[str, Any]:
        decision = self._decision(medium)
        source = decision.get("source") or {}
        audio = decision.get("audio") or []
        return {
            "source_container": source.get("container"),
            "source_video_codec": source.get("video_codec"),
            "source_audio_codec": audio[0].get("codec") if audio else None,
            "source_bitrate_kbps": _kbps(source.get("bitrate")),
            "source_width": source.get("width"),
            "source_height": source.get("height"),
            "source_duration_seconds": _seconds(source.get("duration_ms")),
        }

    def prepare_stream(
        self,
        medium: dict[str, Any],
        scenario: dict[str, Any],
        start_seconds: float,
        trial_id: str,
    ) -> StreamHandle:
        file_id = self._binding(medium)["file_id"]
        if scenario["playback_mode"] == "direct_play":
            return StreamHandle(
                url=self.api_url(f"/files/{file_id}/direct"),
                headers=self.auth_headers,
                session_id=None,
                client_seek_seconds=start_seconds,
                playback_mode="direct_play",
                output_video_codec=medium["video_codec"],
                output_audio_codec=medium["audio_codec"],
                output_bitrate_kbps=medium["bitrate_kbps"],
            )

        response = self.http.json(
            self.api_url(f"/files/{file_id}/hls/sessions"),
            method="POST",
            headers=self.auth_headers,
            json_body={
                "playback_id": f"cinema-plex-bench-{trial_id}",
                "request_id": str(uuid.uuid4()),
                "height": scenario["output_height"],
                "start": start_seconds,
            },
        )
        session_id = response["session_id"]
        rung = next(
            (
                item
                for item in response.get("ladder", [])
                if item.get("height") == scenario["output_height"]
            ),
            None,
        )
        playlist = response.get("playlist_url") or f"/api/v1/hls/{session_id}/index.m3u8"
        if playlist.startswith("/"):
            playlist = self.base + playlist
        return StreamHandle(
            url=playlist,
            headers={},
            session_id=session_id,
            client_seek_seconds=0,
            playback_mode="transcode",
            output_video_codec="h264",
            output_audio_codec="aac",
            output_bitrate_kbps=scenario["output_bitrate_kbps"],
            details={
                "encoder": response.get("encoder"),
                "media_origin_ms": response.get("media_origin_ms"),
                "vod": response.get("vod"),
                "advertised_output_bitrate_kbps": (
                    rung.get("total_kbps") if rung is not None else None
                ),
            },
        )

    def close_stream(self, stream: StreamHandle) -> None:
        if not stream.session_id:
            return
        try:
            self.http.request(
                self.api_url(f"/hls/{stream.session_id}"), method="DELETE", timeout_seconds=10
            )
        except Exception:
            pass


class PlexRunner(ServerRunner):
    product = "plex"

    @property
    def auth_headers(self) -> dict[str, str]:
        return {
            "X-Plex-Token": self.token,
            "X-Plex-Client-Identifier": "cinema-plex-benchmark",
            "X-Plex-Product": "Cinema Plex Benchmark",
            "X-Plex-Version": "1.0",
            "X-Plex-Platform": "Python",
        }

    def _xml(self, path: str) -> ET.Element:
        payload, _, _ = self.http.request(self.base + path, headers=self.auth_headers)
        try:
            return ET.fromstring(payload)
        except ET.ParseError as error:
            raise RuntimeError(f"Plex returned invalid XML for {path}") from error

    def identity(self) -> dict[str, Any]:
        if self._identity is None:
            root = self._xml("/identity")
            self._identity = {
                "server": self.key,
                "server_name": self.config.get("label") or "Plex",
                "server_product": self.product,
                "server_version": root.attrib.get("version"),
                "server_commit": self.config.get("commit"),
                "server_instance_id": root.attrib.get("machineIdentifier"),
            }
        return dict(self._identity)

    def _binding(self, medium: dict[str, Any]) -> dict[str, Any]:
        return medium["bindings"][self.key]

    def _media_nodes(self, medium: dict[str, Any]) -> tuple[ET.Element, ET.Element, ET.Element]:
        binding = self._binding(medium)
        rating_key = urllib.parse.quote(str(binding["rating_key"]), safe="")
        root = self._xml(f"/library/metadata/{rating_key}")
        video = root.find(".//Video")
        if video is None:
            raise RuntimeError(f"Plex rating key {rating_key} has no Video element")
        media_nodes = video.findall("Media")
        media_index = int(binding.get("media_index", 0))
        if media_index >= len(media_nodes):
            raise RuntimeError(f"Plex media_index {media_index} is out of range")
        media_node = media_nodes[media_index]
        part_nodes = media_node.findall("Part")
        part_index = int(binding.get("part_index", 0))
        if part_index >= len(part_nodes):
            raise RuntimeError(f"Plex part_index {part_index} is out of range")
        return video, media_node, part_nodes[part_index]

    def media_info(self, medium: dict[str, Any]) -> dict[str, Any]:
        video, media_node, part = self._media_nodes(medium)
        audio_stream = part.find("Stream[@streamType='2']")
        return {
            "source_container": media_node.attrib.get("container") or part.attrib.get("container"),
            "source_video_codec": media_node.attrib.get("videoCodec"),
            "source_audio_codec": (
                media_node.attrib.get("audioCodec")
                or (audio_stream.attrib.get("codec") if audio_stream is not None else None)
            ),
            "source_bitrate_kbps": _int_or_none(media_node.attrib.get("bitrate")),
            "source_width": _int_or_none(media_node.attrib.get("width")),
            "source_height": _int_or_none(media_node.attrib.get("height")),
            "source_duration_seconds": _milliseconds_attr(video.attrib.get("duration")),
            "part_key": part.attrib.get("key"),
        }

    def prepare_stream(
        self,
        medium: dict[str, Any],
        scenario: dict[str, Any],
        start_seconds: float,
        trial_id: str,
    ) -> StreamHandle:
        binding = self._binding(medium)
        if scenario["playback_mode"] == "direct_play":
            part_key = binding.get("part_key") or self.media_info(medium).get("part_key")
            if not part_key:
                raise RuntimeError("Plex metadata did not expose a direct-play Part key")
            url = part_key if part_key.startswith("http") else self.base + part_key
            return StreamHandle(
                url=url,
                headers=self.auth_headers,
                session_id=None,
                client_seek_seconds=start_seconds,
                playback_mode="direct_play",
                output_video_codec=medium["video_codec"],
                output_audio_codec=medium["audio_codec"],
                output_bitrate_kbps=medium["bitrate_kbps"],
            )

        session_id = f"cinema-plex-bench-{trial_id}-{uuid.uuid4()}"
        height = scenario["output_height"]
        width = max(2, int(round(height * medium["width"] / medium["height"])))
        params = {
            "path": f"/library/metadata/{binding['rating_key']}",
            "mediaIndex": int(binding.get("media_index", 0)),
            "partIndex": int(binding.get("part_index", 0)),
            "protocol": "hls",
            "offset": f"{start_seconds:.3f}",
            "directPlay": 0,
            "directStream": 0,
            "videoQuality": 100,
            "videoResolution": f"{width}x{height}",
            "maxVideoBitrate": scenario["output_bitrate_kbps"],
            "audioBoost": 100,
            "fastSeek": 1,
            "location": "lan",
            "session": session_id,
        }
        url = self.base + "/video/:/transcode/universal/start.m3u8?" + urllib.parse.urlencode(params)
        return StreamHandle(
            url=url,
            headers=self.auth_headers,
            session_id=session_id,
            client_seek_seconds=0,
            playback_mode="transcode",
            output_video_codec="h264",
            output_audio_codec="aac",
            output_bitrate_kbps=scenario["output_bitrate_kbps"],
        )

    def close_stream(self, stream: StreamHandle) -> None:
        if not stream.session_id:
            return
        query = urllib.parse.urlencode({"session": stream.session_id})
        try:
            self.http.request(
                self.base + f"/video/:/transcode/universal/stop?{query}",
                headers=self.auth_headers,
                timeout_seconds=10,
            )
        except Exception:
            pass


def _int_or_none(value: Any) -> int | None:
    try:
        return int(value) if value is not None else None
    except (TypeError, ValueError):
        return None


def _kbps(bits_per_second: Any) -> int | None:
    value = _int_or_none(bits_per_second)
    return int(round(value / 1000)) if value is not None else None


def _seconds(milliseconds: Any) -> float | None:
    value = _int_or_none(milliseconds)
    return value / 1000 if value is not None else None


def _milliseconds_attr(value: Any) -> float | None:
    return _seconds(value)


def build_runners(config: Any, http: HttpClient) -> dict[str, ServerRunner]:
    runners: dict[str, ServerRunner] = {}
    for key, server in config.servers.items():
        token = os.environ.get(server["token_env"])
        if not token:
            raise ConfigError(
                f"environment variable {server['token_env']} is required for server {key!r}"
            )
        cls = CinemaRunner if server["runner"] == "cinema" else PlexRunner
        runners[key] = cls(key, server, token, http)
    return runners
