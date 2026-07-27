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
cargo run -p plurxd                          # serves http://localhost:32400

# 2. Open the web app and create the admin account
open http://localhost:32400                  # first launch = setup screen

# 3. Add a library (Settings → Libraries → Add & scan)
#    Name: Movies   Kind: Movies   Path: /media/movies
#    Kinds: Movies · TV Shows · Anime · Home videos & photos
#    Paths are what the SERVER sees — under Docker that's the CONTAINER path.

# 4. (Optional) add a TMDB key for movie/TV posters
#    Settings → Metadata.  Anime needs no key (AniList), and home libraries
#    use no provider at all — their thumbnails are ffmpeg frame grabs.

# 5. Press play. Open the Stats overlay (ⓘ or press i) to see how it's serving.
```

## 2. Day to day (development)

All developer tasks go through the `Makefile` — CI runs the same targets, so
"green locally" means "green in CI".

```bash
make            # list every target
make run        # serve http://localhost:32400
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
curl -s localhost:32400/healthz            # process alive?
curl -s localhost:32400/readyz             # storage reachable?
curl -s localhost:32400/metrics            # uptime, active transcodes, counts
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

## 4. Pairing another application (monarr)

Two commands. The first is run once by an admin; the second is what the other
application does on every import.

```bash
# 1. Mint a scoped key (with an ADMIN token — not the key you are creating).
#    The secret comes back exactly once; there is no way to read it again.
curl -s localhost:32400/api/v1/keys \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"monarr","scopes":["scan:trigger","status:read"]}'
# → {"id":1,"name":"monarr",…,"key_secret":"plx_…"}   ← paste into monarr

# 2. Tell plurx a file landed. The path is what the PLURX process sees.
curl -s localhost:32400/api/v1/scan \
  -H "Authorization: Bearer plx_…" -H 'Content-Type: application/json' \
  -d '{"path":"/media/movies/Heat (1995)","ids":{"tmdb":949},"hint":"movie",
       "correlation_id":"t-42-a3f9c1","source":"monarr"}'
# → 200 {"status":"scanned","report":{…},"items":[{"item_id":7,…}]}
# → 202 {"status":"queued","request_id":"sr-…"} when that library is busy;
#       poll GET /api/v1/scan/requests/sr-… until "done".
```

| If… | Then… |
|---|---|
| `403` | The key is valid but lacks the scope — mint it with `scan:trigger` |
| `422` listing roots | The path exists for the caller but not for plurx — compare the roots in the response against the two containers' mounts |
| `202` every time | That library is scanning; the request is queued, not lost — poll the `request_id` |
| Item appears, stays named after the file | Enrichment has no TMDB key (Settings → Metadata); the scan itself worked |

Runbook, with monarr's side too: [OPERATIONS.md](OPERATIONS.md).

## Reference — where everything lives

| Thing | Where |
|---|---|
| Web app + API | `http://<host>:32400` |
| GDM discovery | UDP `32414` (movable host-side via `PLURX_GDM_PORT`) |
| Data (db, artwork, transcode cache) | `PLURX_DATA_DIR` (default `./data`; Docker bind mount `${PLURX_DATA:-/srv/plurx}` → `/var/lib/plurx`) |
| Config file | `./plurx.toml` → `/etc/plurx/plurx.toml` (or `PLURX_CONFIG`) |
| Runtime settings (TMDB key, libraries, users) | In the database, edited in Settings — not the config file |
| Deploy templates | [`deploy/`](../deploy) — Compose, systemd, launchd (macOS), Unraid |
| Crates | `plurx-core` (domain + Store) · `plurxd` (HTTP daemon + web app) · `plurx-compat-plex` (Plex façade) |

## Reference — env vars

| Var | Default | Purpose |
|---|---|---|
| `PLURX_BIND` | `0.0.0.0:32400` | HTTP bind address |
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
| `/api/v1/scan` | "Index exactly this path" for another application (scoped key, `scan:trigger`) |
| `/api/v1/keys` | Mint/list/revoke scoped API keys (admin token) |
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
