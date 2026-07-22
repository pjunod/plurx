# Operations — running plurx and reading what it tells you

Companion to [FEATURES.md](FEATURES.md) (what it does) and
[ARCHITECTURE.md](ARCHITECTURE.md) (how it's built) — this is *how to run it day
to day, and what every status, pill, and log line actually means*. For a
copy-paste quickstart see [CHEATSHEET.md](CHEATSHEET.md); for install targets per
platform see [`deploy/README.md`](../deploy/README.md).

The guiding fact for everything below: **paths and hardware are as the server
process sees them, not as you see them.** Most "it found nothing" and "it won't
play" reports are that one gap — a Docker mount, a missing render device — not a
bug.

## Running it

plurx is one static binary, `plurxd`, plus an embedded web app. Three ways to
run it, in order of how most people do:

```bash
# Docker / Compose (recommended for homelabs) — builds from source the first time
cd deploy
cp docker-compose.override.example.yml docker-compose.override.yml   # your mounts + GPU
docker compose up -d --build

# Bare metal — one binary, needs ffmpeg/ffprobe on PATH (or PLURX_FFMPEG/PLURX_FFPROBE)
plurxd run            # serves :32600

# From source (development)
cargo run -p plurxd   # or: make run
```

Open `http://<host>:32600`, create the admin account, add a library. Library
paths you type in the UI are **container-side** paths under Docker (e.g.
`/media/movies`), which must be mounted in your override file. Full deploy matrix
(Unraid, TrueNAS/k8s, ports, GPU passthrough): [`deploy/README.md`](../deploy/README.md).

## Configuration surface

Precedence, lowest to highest: **built-in defaults → TOML file → `PLURX_*` env**.
Settings you edit at runtime (TMDB key, libraries, users) live in the database,
not here — this surface is only what's needed before the database opens.

The TOML file is looked for at `./plurx.toml` then `/etc/plurx/plurx.toml` (or the
path in `PLURX_CONFIG`). Every key has an env override:

| Env var | TOML | Default | What it does |
|---|---|---|---|
| `PLURX_BIND` | `server.bind` | `0.0.0.0:32600` | Address the HTTP API binds to |
| `PLURX_SERVER_NAME` | `server.name` | `plurx` | Human-visible server name |
| `PLURX_DATA_DIR` | `storage.data_dir` | `./data` | Database, artwork, transcode cache (created if missing) |
| `PLURX_CONFIG` | — | — | Explicit config-file path (must exist if set) |
| `PLURX_FFMPEG` | — | `ffmpeg` | ffmpeg binary — point at jellyfin-ffmpeg for best hwaccel |
| `PLURX_FFPROBE` | — | `ffprobe` | ffprobe binary (inspection + chapter markers) |
| `PLURX_HWACCEL` | — | `auto` | Preferred encoder: `auto` · `qsv` · `vaapi` · `nvenc` · `videotoolbox` |
| `PLURX_VAAPI_DEVICE` | — | `/dev/dri/renderD128` | VA-API render node |
| `PLURX_TONEMAP` | — | auto | HDR→SDR tone-map filter override |
| `PLURX_GDM_PORT` | — | `32414` | Host UDP port for GDM discovery (move if Plex owns 32414) |
| `PLURX_LOG` | — | `info` | Log filter (`tracing` EnvFilter syntax, e.g. `plurxd=debug`) |

## Ports

| Port | Proto | Purpose |
|---|---|---|
| 32600 | TCP | HTTP API + web app (and the Plex-compat façade) |
| 32414 | UDP | GDM discovery so Plex/Kodi clients find the server on the LAN |

GDM discovery only works on 32414 (the protocol hard-codes it), but the *host*
port is movable via `PLURX_GDM_PORT` when a still-running Plex owns it — you lose
LAN auto-discovery on that host port, not the server.

## Reading the activity pill

Every page shows a pill of what the server is doing right now. Empty = idle and
hidden. `Scanning Movies · 182 / 210 files · 86%` means a scan is in its file
pass; it flips to `fetching metadata…` for enrichment, then disappears. It's a
live read of server-side job state, not a client guess — if it's spinning,
something is actually running; if a scan looks stuck, the pill (and the logs)
will say where.

## Reading library scan status

In Settings → Libraries, the Status column is the truth about each library:

| Status | Meaning | What to check |
|---|---|---|
| `idle` | No scan running; last scan finished | Item count looks right? |
| `scanning… N / M files` | File pass in progress | — |
| `fetching metadata…` | Files done, enrichment running | TMDB key set? |
| `error: …` (red) | The scan failed, with the reason | Almost always a path the **server** can't see |

**How to read it:** the single most common failure is a library that scans `0`
files while you can see the folder full of media. That means the path you typed
isn't the path the server process has — under Docker, the container-side mount
path must match. Fix the mount, not the library name.

## Reading the Server card (Settings)

The Server card is the health-at-a-glance panel:

- **ffmpeg** — the version string if it ran, or a red "not found" if the binary
  is missing. Red here means scanning and transcoding will both fail; fix
  `PLURX_FFMPEG` before anything else.
- **Hardware** — a pill per encoder (NVENC · QuickSync · VA-API · VideoToolbox),
  green ✓ if startup validation test-encoded through it, grey — if not
  available. Startup validation actually runs a probe encode, so a green pill
  means it worked once, not that a driver merely exists.
- **Transcoder** — the encoder the server will actually pick, and your preference
  (`PLURX_HWACCEL`). If you set `qsv` but see software selected, the QSV probe was
  rejected — the log line says why.
- **Right now** — active streams, library count, user count.

## Reading playback (and the stats overlay)

Every playback resolves to one of three methods; open the player **Stats**
overlay (the ⓘ button, or press `i`) to see which:

- **Direct play** — the file is sent untouched. Ideal; zero transcode CPU.
- **Remux** — container repackaged, `-c:v copy`. Cheap; a little CPU for audio at
  most.
- **Transcode · QuickSync** (or NVENC/VA-API/VideoToolbox) — the GPU is
  re-encoding, usually because of codec/resolution/HDR the device can't take.
- **Transcode · software** — it fell back to CPU x264. Expected on a first-gen or
  driver-mismatched GPU, or as the self-heal after a hardware session stalled —
  check the logs for the rejection reason.

The overlay's **Source** vs **Now decoding** lines are the useful comparison:
Source is what the file is (from the server's probe); Now decoding is what your
browser is actually rendering. A 3840×2160 source showing 1920×1080 now-decoding
is a working downscale transcode.

## Logs

Structured `tracing` logs go to stdout/journald, and the same buffer is exposed
in Settings → Logs with a level filter (`info` / `warn` / `error` / `debug`) and
auto-refresh. Each line is `time  level  target — message`. The targets that
matter most: `plurxd::scan` (library passes), `plurxd::meta` (provider matches),
`plurxd::transcode` (encoder selection + why a hardware path was rejected),
`plurxd::stream` (session lifecycle). Raise verbosity for one subsystem with
`PLURX_LOG=plurxd::transcode=debug`.

## Health & metrics

| Endpoint | Use |
|---|---|
| `GET /healthz` | Liveness — the process is up |
| `GET /readyz` | Readiness — storage is reachable (use for load-balancer health) |
| `GET /metrics` | Prometheus text: uptime, active transcode sessions, library and user counts |

## Hardware transcode & recent Intel GPUs

The Docker image defaults to **jellyfin-ffmpeg**, which bundles a current Intel
media driver + libva + oneVPL. This matters for newer silicon: an Arc / Meteor
Lake / **Arrow Lake** iGPU (on the kernel `xe` driver) is years newer than the VA
driver Debian ships, so the distro ffmpeg fails VA-API init with an I/O error
while jellyfin-ffmpeg drives it fine. Pass the GPU through and add the render
group in your compose override:

```yaml
    devices:
      - /dev/dri:/dev/dri
    group_add:
      - "992"          # stat -c '%g' /dev/dri/renderD128 on the host
```

On Intel **Arc**-class GPUs, QuickSync (oneVPL) is usually more reliable than
VA-API — set `PLURX_HWACCEL: "qsv"`. Two concurrent QSV sessions on one iGPU can
stall; plurx's watchdog catches that and self-heals to software x264 (you'll see
the loading overlay a few seconds longer, then playback).

## Common problems → cause

| Symptom | Cause | Fix |
|---|---|---|
| Scan finds `0` files | Path isn't what the **server** sees | Match the Docker mount to the library path |
| `error: … ` on a library | Server can't read the path | Check mount, permissions, that the share is mounted |
| Item won't play, shows a missing-file notice | File not on disk (unmounted share) | Remount; plurx correctly refuses to open a dead player |
| `docker … pull access denied` | Image isn't published under that name | Build from source: `docker compose up -d --build` |
| GDM won't bind / port conflict | A running Plex owns UDP 32414 | Set `PLURX_GDM_PORT`, or stop Plex |
| Gray screen then playback | Hardware session stalled, watchdog fell back to software | Expected under concurrency; check `PLURX_HWACCEL` |
| 4K HDR / Dolby Vision won't play | Heavy HEVC is hardware-decoded (Intel too); if the GPU can't decode it and software can't either, the session now fails fast with a clear log line instead of hanging gray | Read `plurxd::transcode` — the last ffmpeg line names the real cause (decode vs tone-map). DV profile 5 is the hardest case |
| Playback is software when you set `qsv` | The QSV probe was rejected at startup | Read `plurxd::transcode` logs; usually a driver/`/dev/dri` gap |
| No posters, just filenames | No TMDB key (movies/TV) | Add a key in Settings → Metadata (anime needs none) |
