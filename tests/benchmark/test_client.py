"""FFmpeg deadline and subprocess-cleanup regression tests."""

import os
from pathlib import Path
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]

# How long a grandchild holds an inherited stdout write end open. It only has to
# outlast the client's own teardown by enough that no amount of machine load can
# reorder the two, so this is a margin, not a measured budget.
HOLD_SECONDS = 30
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
            root = Path(directory)
            holder_pid_path = root / "holder-pid"
            released_path = root / "released"
            holder = root / "stdout-holder.py"
            holder.write_text(
                "import time\n"
                f"time.sleep({HOLD_SECONDS})\n"
                f"open({str(released_path)!r}, 'w', encoding='utf-8').write('released')\n",
                encoding="utf-8",
            )
            executable = root / "held-stdout-decoder"
            executable.write_text(
                "#!/usr/bin/env python3\n"
                "import subprocess, sys\n"
                "print('out_time_us=1000', flush=True)\n"
                f"holder = subprocess.Popen([sys.executable, {str(holder)!r}])\n"
                f"open({str(holder_pid_path)!r}, 'w', encoding='utf-8')"
                ".write(str(holder.pid))\n",
                encoding="utf-8",
            )
            executable.chmod(0o755)
            measured = client(executable, HOLD_SECONDS).decode_for(stream(), 0.01)
            self.assertIn("first_frame_monotonic", measured)
            # The decoder exits immediately, so its own exit is what the client
            # waits on; the grandchild keeps the inherited stdout write end open
            # for HOLD_SECONDS. Both facts below are ordering, not timing: the
            # grandchild writes `released` only after that hold elapses, and the
            # pipe reaches EOF only after the grandchild is gone. A client that
            # waited for EOF could not return before `released` exists.
            self.assertTrue(
                holder_pid_path.exists(),
                "decoder never spawned the stdout-holding grandchild",
            )
            self.assertFalse(
                released_path.exists(),
                "client waited for the inherited stdout write end to close",
            )

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
