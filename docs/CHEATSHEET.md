# Cheat sheet — what to type, in what order

Companion to [OPERATIONS.md](OPERATIONS.md) (what each output means) — this is
the fast path: the commands to run and a reference table of where everything
lives. Numbered sections happen in order; the reference tables are consulted as
needed.

## 1. First run — from zero to playing

```bash
# 1. Start the server (pick one)
cd deploy && cp docker-compose.override.example.yml docker-compose.override.yml
$EDITOR docker-compose.override.yml         # set your media mounts (host:container:ro) + GPU
docker compose up -d --build                # builds from source the first time
# ...or bare metal / dev:
cargo run -p plurxd                          # serves http://localhost:32600

# 2. Open the web app and create the admin account
open http://localhost:32600                  # first launch = setup screen

# 3. Add a library (Settings → Libraries → Add & scan)
#    Name: Movies   Kind: Movies   Path: /media/movies
#    Paths are what the SERVER sees — under Docker that's the CONTAINER path.

# 4. (Optional) add a TMDB key for movie/TV posters
#    Settings → Metadata.  Anime needs no key (AniList).

# 5. Press play. Open the Stats overlay (ⓘ or press i) to see how it's serving.
```

## 2. Day to day (development)

All developer tasks go through the `Makefile` — CI runs the same targets, so
"green locally" means "green in CI".

```bash
make            # list every target
make run        # serve http://localhost:32600
make check      # fmt-check + clippy + test  (the CI gate — the single quality bar)
make test       # just the tests
make coverage   # line coverage via cargo-llvm-cov → lcov.info
make hooks      # install a pre-commit hook that runs `make check`
make docker     # build the container image
```

`make check` is the single source of truth: `make hooks` wires it into a
pre-commit hook so a commit can't land unless it passes (bypass one commit with
`git commit --no-verify`). Pushing a version tag (`git tag v0.1.0 && git push
--tags`) builds and publishes a multi-arch image to `ghcr.io/pjunod/plurx`.

## 3. When something's off (quick triage)

```bash
curl -s localhost:32600/healthz            # process alive?
curl -s localhost:32600/readyz             # storage reachable?
curl -s localhost:32600/metrics            # uptime, active transcodes, counts
docker compose logs -f plurxd              # or: journalctl -u plurxd -f
PLURX_LOG=plurxd::transcode=debug plurxd run   # loud logs for one subsystem
stat -c '%g' /dev/dri/renderD128           # the render group id for group_add
```

| If… | Then… |
|---|---|
| Scan finds 0 files | The path isn't what the **server** sees — match the Docker mount |
| Won't play, "file missing" | The share is unmounted; remount (plurx refuses a dead player on purpose) |
| `pull access denied` | Build from source: `docker compose up -d --build` |
| GDM port conflict | `PLURX_GDM_PORT=<n>` (a running Plex owns 32414) |
| Playback is software, you set `qsv` | Read `plurxd::transcode` logs; usually a `/dev/dri`/driver gap |

Full symptom→cause table: [OPERATIONS.md](OPERATIONS.md#common-problems--cause).

## Reference — where everything lives

| Thing | Where |
|---|---|
| Web app + API | `http://<host>:32600` |
| GDM discovery | UDP `32414` (movable host-side via `PLURX_GDM_PORT`) |
| Data (db, artwork, transcode cache) | `PLURX_DATA_DIR` (default `./data`; Docker volume `plurx-data` → `/var/lib/plurx`) |
| Config file | `./plurx.toml` → `/etc/plurx/plurx.toml` (or `PLURX_CONFIG`) |
| Runtime settings (TMDB key, libraries, users) | In the database, edited in Settings — not the config file |
| Deploy templates | [`deploy/`](../deploy) — Compose, systemd, Unraid |
| Crates | `plurx-core` (domain + Store) · `plurxd` (HTTP daemon + web app) · `plurx-compat-plex` (Plex façade) |

## Reference — env vars

| Var | Default | Purpose |
|---|---|---|
| `PLURX_BIND` | `0.0.0.0:32600` | HTTP bind address |
| `PLURX_DATA_DIR` | `./data` | Database + caches |
| `PLURX_SERVER_NAME` | `plurx` | Server display name |
| `PLURX_CONFIG` | — | Explicit config path |
| `PLURX_FFMPEG` / `PLURX_FFPROBE` | `ffmpeg` / `ffprobe` | Media tools (use jellyfin-ffmpeg) |
| `PLURX_HWACCEL` | `auto` | `auto` · `qsv` · `vaapi` · `nvenc` · `videotoolbox` |
| `PLURX_VAAPI_DEVICE` | `/dev/dri/renderD128` | VA-API render node |
| `PLURX_TONEMAP` | auto | HDR→SDR filter override |
| `PLURX_GDM_PORT` | `32414` | GDM host port |
| `PLURX_TRAKT_BASE` | `https://api.trakt.tv` | Trakt API base (tests/mocks) |
| `PLURX_LOG` | `info` | `tracing` filter (e.g. `plurxd=debug`) |

## Reference — health & API surfaces

| Path | Purpose |
|---|---|
| `GET /healthz` | Liveness |
| `GET /readyz` | Storage reachable (load-balancer health) |
| `GET /metrics` | Prometheus metrics |
| `/api/v1/...` | Native JSON API (bearer token) |
| `/api/v1/files/{id}/decision` | How a file will be served (direct / remux / transcode) + markers |
| Plex-compat façade | `/identity`, `/library/...`, `/:/timeline`, GDM — for Kodi-family Plex clients |

## Reference — player keyboard & controls

| Key / control | Action |
|---|---|
| `i` | Toggle the stats overlay |
| `Esc` | Close the player (or exit fullscreen first) |
| ⓘ Stats | Same as `i` |
| 🔊 Audio / 💬 Subtitles | Track menus (shown when there's more than one) |
| Skip Intro / Skip Credits | Appear when playback enters a marked region |
| Preferences (◐) → Playback | Auto-skip intro & credits toggle |
