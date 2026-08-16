"""Common FFmpeg decoder used for end-to-end first-frame measurements."""

from __future__ import annotations

import os
import queue
import shutil
import signal
import subprocess
import threading
import time
from typing import Any

from .runners import StreamHandle


class ClientError(RuntimeError):
    pass


class FfmpegClient:
    def __init__(self, executable: str, timeout_seconds: float, secrets: list[str]):
        resolved = shutil.which(executable)
        if not resolved:
            raise ClientError(f"FFmpeg client executable not found: {executable}")
        self.executable = resolved
        self.timeout_seconds = timeout_seconds
        self.secrets = [secret for secret in secrets if secret]
        version = subprocess.run(
            [resolved, "-version"], capture_output=True, text=True, timeout=10, check=False
        )
        if version.returncode != 0:
            raise ClientError("FFmpeg client failed its version probe")
        self.version = (version.stdout.splitlines() or ["ffmpeg"])[0]

    def _redact(self, text: str) -> str:
        clean = text
        for secret in self.secrets:
            clean = clean.replace(secret, "<redacted>")
        return clean

    @staticmethod
    def _header_arg(headers: dict[str, str]) -> list[str]:
        if not headers:
            return []
        joined = "".join(f"{key}: {value}\r\n" for key, value in headers.items())
        return ["-headers", joined]

    def _input_args(self, stream: StreamHandle, *, realtime: bool = False) -> list[str]:
        args: list[str] = []
        if realtime:
            args.append("-re")
        args.extend(self._header_arg(stream.headers))
        if stream.client_seek_seconds > 0:
            args.extend(["-ss", f"{stream.client_seek_seconds:.3f}"])
        args.extend(["-i", stream.url])
        return args

    def decode_first_frame(self, stream: StreamHandle) -> dict[str, Any]:
        return self._one_video_unit(stream, decode=True)

    def read_first_packet(self, stream: StreamHandle) -> dict[str, Any]:
        """Seek and demux one source packet without charging video decode."""
        return self._one_video_unit(stream, decode=False)

    def _one_video_unit(self, stream: StreamHandle, *, decode: bool) -> dict[str, Any]:
        command = [
            self.executable,
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            *self._input_args(stream),
            "-map",
            "0:v:0",
            "-an",
            *([] if decode else ["-c:v", "copy"]),
            "-frames:v",
            "1",
            "-f",
            "null",
            "-",
        ]
        try:
            result = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=self.timeout_seconds,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise ClientError(f"no decoded frame within {self.timeout_seconds:.1f}s") from error
        if result.returncode != 0:
            detail = self._redact(result.stderr).strip().splitlines()[-8:]
            action = "decode a frame" if decode else "demux a packet"
            raise ClientError(f"FFmpeg could not {action}: " + " | ".join(detail))
        return {"decoded_frames" if decode else "demuxed_packets": 1}

    def decode_for(self, stream: StreamHandle, observe_seconds: float) -> dict[str, Any]:
        """Decode at realtime and retain the stream for a concurrency window."""
        command = [
            self.executable,
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-stats_period",
            "0.05",
            "-progress",
            "pipe:1",
            *self._input_args(stream, realtime=True),
            "-map",
            "0:v:0",
            "-an",
            "-t",
            f"{observe_seconds:.3f}",
            "-f",
            "null",
            "-",
        ]
        started = time.monotonic()
        try:
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        except OSError as error:
            raise ClientError(f"could not start FFmpeg: {error}") from error
        first_progress: float | None = None
        assert process.stdout is not None
        try:
            for line in process.stdout:
                if line.startswith(("out_time_us=", "out_time_ms=")):
                    try:
                        media_time = int(line.split("=", 1)[1])
                    except ValueError:
                        continue
                    if media_time > 0 and first_progress is None:
                        first_progress = time.monotonic()
            remaining = max(0.1, self.timeout_seconds - (time.monotonic() - started))
            process.wait(timeout=remaining)
        except subprocess.TimeoutExpired as error:
            process.kill()
            process.wait()
            raise ClientError(f"decoder did not finish within {self.timeout_seconds:.1f}s") from error
        if process.returncode != 0 or first_progress is None:
            detail = self._redact((process.stderr.read() if process.stderr else "")).strip().splitlines()[-8:]
            raise ClientError("FFmpeg concurrency client failed: " + " | ".join(detail))
        return {"first_frame_monotonic": first_progress, "decoded_seconds": observe_seconds}

    def pause_resume(
        self, stream: StreamHandle, warmup_seconds: float, pause_seconds: float
    ) -> dict[str, Any]:
        if not hasattr(signal, "SIGSTOP") or not hasattr(signal, "SIGCONT"):
            raise ClientError("pause/resume measurement requires POSIX SIGSTOP/SIGCONT")
        command = [
            self.executable,
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-stats_period",
            "0.05",
            "-progress",
            "pipe:1",
            *self._input_args(stream, realtime=True),
            "-map",
            "0:v:0",
            "-an",
            "-f",
            "null",
            "-",
        ]
        try:
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        except OSError as error:
            raise ClientError(f"could not start FFmpeg: {error}") from error
        assert process.stdout is not None
        events: queue.Queue[tuple[float, float]] = queue.Queue()

        def read_progress() -> None:
            for line in process.stdout:
                if not line.startswith(("out_time_us=", "out_time_ms=")):
                    continue
                try:
                    events.put((time.monotonic(), int(line.split("=", 1)[1]) / 1_000_000))
                except ValueError:
                    continue

        reader = threading.Thread(target=read_progress, daemon=True)
        reader.start()
        deadline = time.monotonic() + self.timeout_seconds
        last_media = 0.0
        try:
            while time.monotonic() < deadline and last_media < warmup_seconds:
                _, last_media = events.get(timeout=max(0.01, min(1, deadline - time.monotonic())))
            if last_media < warmup_seconds:
                raise ClientError("decoder ended before pause warmup completed")
            os.kill(process.pid, signal.SIGSTOP)
            time.sleep(pause_seconds)
            while not events.empty():
                try:
                    events.get_nowait()
                except queue.Empty:
                    break
            resumed = time.monotonic()
            os.kill(process.pid, signal.SIGCONT)
            first_after: float | None = None
            while time.monotonic() < deadline:
                at, media_time = events.get(timeout=max(0.01, min(1, deadline - time.monotonic())))
                if media_time > last_media:
                    first_after = at
                    break
            if first_after is None:
                raise ClientError("decoder produced no frame after resume")
            return {
                "resume_latency_ms": (first_after - resumed) * 1000,
                "pause_seconds": pause_seconds,
                "warmup_seconds": warmup_seconds,
            }
        except queue.Empty as error:
            raise ClientError("decoder progress timed out during pause/resume") from error
        finally:
            if process.poll() is None:
                try:
                    os.kill(process.pid, signal.SIGCONT)
                except ProcessLookupError:
                    pass
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
