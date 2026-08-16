"""FFmpeg deadline and subprocess-cleanup regression tests."""

import os
from pathlib import Path
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from cinema_plex_bench.client import ClientError, FfmpegClient  # noqa: E402
from cinema_plex_bench.runners import StreamHandle  # noqa: E402


def stream():
    return StreamHandle(
        url="http://unused.invalid/output.m3u8",
        headers={},
        session_id="session",
        client_seek_seconds=0,
        playback_mode="transcode",
        output_video_codec="h264",
        output_audio_codec="aac",
        output_bitrate_kbps=8000,
        output_height=1080,
    )


def client(executable: Path, timeout_seconds: float) -> FfmpegClient:
    instance = object.__new__(FfmpegClient)
    instance.executable = str(executable)
    instance.probe_executable = str(executable)
    instance.timeout_seconds = timeout_seconds
    instance.secrets = []
    return instance


class ClientTests(unittest.TestCase):
    def test_silent_decoder_obeys_deadline_and_is_reaped(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pid_path = root / "pid"
            executable = root / "silent-decoder"
            executable.write_text(
                "#!/bin/sh\n"
                f"printf '%s\\n' \"$$\" > {str(pid_path)!r}\n"
                "sleep 10\n",
                encoding="utf-8",
            )
            executable.chmod(0o755)
            started = time.monotonic()
            with self.assertRaisesRegex(ClientError, "did not finish"):
                client(executable, 0.5).decode_for(stream(), 1)
            self.assertLess(time.monotonic() - started, 1)
            pid = int(pid_path.read_text(encoding="utf-8"))
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    def test_parent_exit_does_not_wait_for_inherited_stdout_eof(self):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "held-stdout-decoder"
            executable.write_text(
                "#!/usr/bin/env python3\n"
                "import subprocess, sys\n"
                "print('out_time_us=1000', flush=True)\n"
                "subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(0.5)'])\n",
                encoding="utf-8",
            )
            executable.chmod(0o755)
            started = time.monotonic()
            measured = client(executable, 1).decode_for(stream(), 0.01)
            self.assertLess(time.monotonic() - started, 0.45)
            self.assertIn("first_frame_monotonic", measured)

    def test_signal_resistant_decoder_is_killed_without_a_five_second_overrun(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pid_path = root / "pid"
            executable = root / "signal-resistant-decoder"
            executable.write_text(
                "#!/bin/sh\n"
                "trap '' TERM\n"
                f"printf '%s\\n' \"$$\" > {str(pid_path)!r}\n"
                "while :; do sleep 10; done\n",
                encoding="utf-8",
            )
            executable.chmod(0o755)
            started = time.monotonic()
            with self.assertRaisesRegex(ClientError, "did not finish"):
                client(executable, 0.5).decode_for(stream(), 1)
            self.assertLess(time.monotonic() - started, 1)
            pid = int(pid_path.read_text(encoding="utf-8"))
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)


if __name__ == "__main__":
    unittest.main()
