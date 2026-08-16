"""Common FFmpeg decoder used for end-to-end first-frame measurements."""

from __future__ import annotations

from collections import deque
import json
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
    def __init__(
        self,
        executable: str,
        timeout_seconds: float,
        secrets: list[str],
        probe_executable: str = "ffprobe",
    ):
        resolved = shutil.which(executable)
        if not resolved:
            raise ClientError(f"FFmpeg client executable not found: {executable}")
        self.executable = resolved
        resolved_probe = shutil.which(probe_executable)
        if not resolved_probe:
            raise ClientError(f"FFprobe executable not found: {probe_executable}")
        self.probe_executable = resolved_probe
        self.timeout_seconds = timeout_seconds
        self.secrets = [secret for secret in secrets if secret]
        version = subprocess.run(
            [resolved, "-version"], capture_output=True, text=True, timeout=10, check=False
        )
        if version.returncode != 0:
            raise ClientError("FFmpeg client failed its version probe")
        self.version = (version.stdout.splitlines() or ["ffmpeg"])[0]
        probe_version = subprocess.run(
            [resolved_probe, "-version"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        if probe_version.returncode != 0:
            raise ClientError("FFprobe failed its version probe")
        self.probe_version = (probe_version.stdout.splitlines() or ["ffprobe"])[0]

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
            process = subprocess.Popen(
                command,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=os.name == "posix",
            )
        except OSError as error:
            raise ClientError(f"could not start FFmpeg: {error}") from error
        first_progress: float | None = None
        assert process.stdout is not None
        assert process.stderr is not None
        progress: queue.Queue[tuple[float, int]] = queue.Queue()
        stderr_tail: deque[str] = deque(maxlen=8)

        def read_progress() -> None:
            for line in process.stdout:
                if not line.startswith(("out_time_us=", "out_time_ms=")):
                    continue
                try:
                    progress.put((time.monotonic(), int(line.split("=", 1)[1])))
                except ValueError:
                    continue

        def read_stderr() -> None:
            for line in process.stderr:
                stderr_tail.append(line.rstrip())

        progress_reader = threading.Thread(target=read_progress, daemon=True)
        stderr_reader = threading.Thread(target=read_stderr, daemon=True)
        progress_reader.start()
        stderr_reader.start()
        deadline = started + self.timeout_seconds
        try:
            while process.poll() is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ClientError(
                        f"decoder did not finish within {self.timeout_seconds:.1f}s"
                    )
                try:
                    at, media_time = progress.get(timeout=min(0.1, remaining))
                except queue.Empty:
                    continue
                if media_time > 0 and first_progress is None:
                    first_progress = at
            while True:
                try:
                    at, media_time = progress.get_nowait()
                except queue.Empty:
                    break
                if media_time > 0 and first_progress is None:
                    first_progress = at
        finally:
            self._stop_process(
                process,
                grace_seconds=max(0, min(0.25, deadline - time.monotonic())),
            )
            progress_reader.join(timeout=0.05)
            stderr_reader.join(timeout=0.05)
            if progress_reader.is_alive() or stderr_reader.is_alive():
                self._signal_process_group(process, signal.SIGTERM)
                progress_reader.join(timeout=0.2)
                stderr_reader.join(timeout=0.2)
            if progress_reader.is_alive() or stderr_reader.is_alive():
                self._signal_process_group(process, signal.SIGKILL)
                progress_reader.join(timeout=0.2)
                stderr_reader.join(timeout=0.2)
            process.stdout.close()
            process.stderr.close()
        while True:
            try:
                at, media_time = progress.get_nowait()
            except queue.Empty:
                break
            if media_time > 0 and first_progress is None:
                first_progress = at
        if process.returncode != 0 or first_progress is None:
            detail = [self._redact(line) for line in stderr_tail]
            raise ClientError("FFmpeg concurrency client failed: " + " | ".join(detail))
        return {"first_frame_monotonic": first_progress, "decoded_seconds": observe_seconds}

    @staticmethod
    def _signal_process_group(process: subprocess.Popen[str], sig: int) -> None:
        try:
            if os.name == "posix":
                os.killpg(process.pid, sig)
            elif process.poll() is None:
                process.send_signal(sig)
        except ProcessLookupError:
            pass
        except PermissionError:
            if process.poll() is None:
                process.send_signal(sig)

    @classmethod
    def _stop_process(
        cls, process: subprocess.Popen[str], grace_seconds: float = 0.25
    ) -> None:
        if process.poll() is not None:
            process.wait()
            return
        cls._signal_process_group(process, signal.SIGTERM)
        try:
            process.wait(timeout=max(0, grace_seconds))
        except subprocess.TimeoutExpired:
            cls._signal_process_group(process, signal.SIGKILL)
            process.wait()

    def probe_stream(self, stream: StreamHandle) -> dict[str, Any]:
        """Read the delivered codecs and dimensions outside benchmark clocks."""
        command = [
            self.probe_executable,
            "-v",
            "error",
            *self._header_arg(stream.headers),
            "-read_intervals",
            "%+#1",
            "-show_entries",
            "stream=codec_type,codec_name,width,height",
            "-of",
            "json",
            stream.url,
        ]
        try:
            result = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=self.timeout_seconds,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise ClientError(f"FFprobe output-contract probe failed: {error}") from error
        if result.returncode != 0:
            detail = self._redact(result.stderr).strip().splitlines()[-8:]
            raise ClientError("FFprobe output-contract probe failed: " + " | ".join(detail))
        try:
            document = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise ClientError("FFprobe output-contract probe returned invalid JSON") from error
        streams = document.get("streams") if isinstance(document, dict) else None
        if not isinstance(streams, list):
            raise ClientError("FFprobe output-contract probe returned no streams")
        video = next(
            (item for item in streams if item.get("codec_type") == "video"), None
        )
        audio = next(
            (item for item in streams if item.get("codec_type") == "audio"), None
        )
        if not isinstance(video, dict) or not isinstance(audio, dict):
            raise ClientError("transcode output must contain video and audio streams")
        return {
            "probed_output_video_codec": video.get("codec_name"),
            "probed_output_audio_codec": audio.get("codec_name"),
            "probed_output_width": video.get("width"),
            "probed_output_height": video.get("height"),
        }

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
            process = subprocess.Popen(
                command,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=os.name == "posix",
            )
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
            self._stop_process(
                process,
                grace_seconds=max(0, min(0.25, deadline - time.monotonic())),
            )
            reader.join(timeout=0.2)
            process.stdout.close()
            if process.stderr is not None:
                process.stderr.close()
