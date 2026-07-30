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
cd .. && make deploy      # builds + starts, and stamps the commit so the server can name it

# Bare metal — one binary, needs ffmpeg/ffprobe on PATH (or PLURX_FFMPEG/PLURX_FFPROBE)
plurxd run            # serves :32400

# From source (development)
cargo run -p plurxd   # or: make run
```

Open `http://<host>:32400`, create the admin account, add a library. Library
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
| `PLURX_BIND` | `server.bind` | `0.0.0.0:32400` | Address the HTTP API binds to |
| `PLURX_SERVER_NAME` | `server.name` | `plurx` | Human-visible server name |
| `PLURX_DATA_DIR` | `storage.data_dir` | `./data` | Database, artwork, transcode cache (created if missing) |
| `PLURX_CONFIG` | — | — | Explicit config-file path (must exist if set) |
| `PLURX_FFMPEG` | — | `ffmpeg` | ffmpeg binary — point at jellyfin-ffmpeg for best hwaccel |
| `PLURX_FFPROBE` | — | `ffprobe` | ffprobe binary (inspection + chapter markers) |
| `PLURX_HWACCEL` | — | `auto` | Preferred encoder: `auto` · `qsv` · `vaapi` · `nvenc` · `videotoolbox` |
| `PLURX_VAAPI_DEVICE` | — | `/dev/dri/renderD128` | VA-API render node |
| `PLURX_TONEMAP` | — | zscale | The **CPU** tone-map operator: `zscale` · `libplacebo` · `off` (no tone-map — plays HDR washed, but a useful test/escape hatch). Which *pipeline* runs — GPU or CPU — is probed at boot, not configured; this only chooses the operator when the CPU chain is the one running |
| `PLURX_HWDECODE` | — | on | Set `off` to force software decode (still hardware-encodes) — for a GPU that decodes a stream to garbage, e.g. some Dolby Vision |
| `PLURX_GDM_PORT` | — | `32414` | Host UDP port for GDM discovery (move if Plex owns 32414) |
| `PLURX_LOG` | — | `info` | Log filter (`tracing` EnvFilter syntax, e.g. `plurxd=debug`) |

## Ports

| Port | Proto | Purpose |
|---|---|---|
| 32400 | TCP | HTTP API + web app (and the Plex-compat façade) |
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
- **Storage** — what each library's storage reads at, and what a cold seek into
  a file costs there. See below; it is the one number on this card that can
  change without a restart, so it has a **Re-measure** button.
- **Right now** — active streams, library count, user count.

## Reading the storage numbers

A remux and a direct play both send the source's own video untouched, so the
storage has to sustain that file's bitrate for the whole film — and the
transcode paths have to read it at least that fast to keep an encoder fed.
When storage cannot, there is nothing in the playback UI that says so: the
viewer sees a `supply` stall, the Stats overlay shows the encoder under 1×,
and both readings are true while pointing away from the cause.

The probe answers it directly. It runs a few seconds after boot (so a sleeping
array never delays startup) and again whenever you press **Re-measure** or
`POST /api/v1/system/storage`. Per mount — libraries on the same device are
measured once, together — it reports:

- **Read rate**, in Mb/s, the same unit as a file's bitrate. A file whose
  bitrate is above this cannot play from here, whatever the encoder or the
  client does. A file above `read rate ÷ stream_readrate` plays but never
  builds a reserve, so any hiccup on the link becomes a stall.
- **Cold seek**, the median time to the first bytes at an untouched offset.
  Single-digit milliseconds is local storage; tens of milliseconds is a
  network mount, and it is what every scrub and every resume pays before
  ffmpeg can emit a frame.

It reads a real media file from the library rather than a fixture it wrote —
a fixture would still be in the page cache and would report a NAS at several
GB/s. If a number still comes back impossibly high the card says so rather
than quietly boasting; treat that mount's figure as a floor.

`scripts/perf-report` prints the same numbers with the comparison already
done, which is usually the faster way to answer "is my storage the problem".

### When the average looks fine and it still stalls

An average cannot show a gap, and a gap is what stalls a viewer: a stretch
where supply fell below demand for longer than the client's buffer covers. A
mount reading 250 Mb/s has ample room for a 69 Mb/s film on paper and can
still stall it several times an hour.

`scripts/perf-report --sustained` answers that. It reads continuously for 30
seconds per mount (pass a number for longer, up to 120), samples every 250 ms,
and reports the distribution — min, p10, median, max — instead of one figure.
It then *replays* that trace against a simulated client at a ladder of source
bitrates and two buffer depths, and prints what would have happened:

```
        source         2.2s buffer     15.0s buffer
          75 Mb/s    3 stalls/6s dry   ok (low 4.2s)
```

The left column is the progressive `/stream.mp4` path, whose read-ahead Chrome
caps at about 2.2 seconds; the right is what an MSE path (hls.js — your
transcodes and cache hits) holds. Same trace, same file, different outcome. A
row that stalls on the left and not the right means the storage is fine and the
delivery path is the problem.

`ok` is not automatically comfortable — the low-water figure in brackets is how
close the buffer came to empty. `ok (low 0.2s)` is a near miss.

It costs real reading: seconds × the read rate × the number of mounts. Run it
when something is wrong, not on a schedule.

## Reading playback (and the stats overlay)

The overlay's first row is **Build** — which binary is answering, as
`0.2.0 · v0.2.0-7-g0e80e42`, or `0.2.0 · built 30 Jul 16:35Z` when the image
was made from a context with no commit in it (deploy with `make deploy` and it
can name the commit). It is there because "which build is this?" is the first
question of any debugging session, and the answer used to live behind an admin
login on the System page — invisible in every screenshot anybody sends. The web
UI is compiled into the binary, so that one line covers both halves.

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

### Delivery speed

A remux is a copy: ffmpeg can read it off disk and push it into the socket far
faster than it plays — over 200× real time on a local link. Left alone it will
take the whole link for as long as the burst lasts, and since every seek opens a
fresh stream, scrubbing means doing that repeatedly. On wired gigabit that is
merely impolite. Over Wi-Fi it monopolises airtime, and a client that happens to
need its DHCP lease renewed mid-burst can lose the lease and then fail to get it
back, because the broadcast `DISCOVER` goes out at the lowest basic rate and is
the first thing a saturated AP drops.

**Settings → Playback → Delivery speed** bounds it. The first 30 seconds of any
stream always arrive flat-out, so starting and seeking stay instant; the limit
applies after that.

| Setting | Use when |
|---|---|
| 2× | Marginal Wi-Fi, a powerline/MoCA bridge, or a link shared with anything latency-sensitive |
| 4× (default) | Anything normal. Absorbs the peaks of a variable-bitrate film and keeps building buffer |
| 8× | Fast wired LAN where you want a deeper buffer sooner |
| Unlimited | Wired-only, and only if you actively want the old behaviour |

Below 1× is refused: a stream delivered slower than it plays can never buffer,
so playback would stall by construction.

The limit needs ffmpeg 5.1 or newer (`-readrate`), and the initial burst needs
6.1 (`-readrate_initial_burst`). plurx probes for both at startup and logs a
warning if the ffmpeg it found has neither, in which case streams run unpaced
whatever this is set to.

### Transcode buffering

Delivery speed above governs the progressive remux, which the browser pulls at
its own pace. An HLS session — a transcode, or the copy-video repackaging
Safari and Apple TV get — is different: ffmpeg writes segments to disk and the
player fetches them, so *the server decides how much buffer the viewer is
allowed to have.* **Settings → Playback → Transcode buffering** is that
decision, in three parts.

| Control | Setting key | Default | What it does |
|---|---|---|---|
| Head start | `playback.hls_burst_secs` | 90 s | Content delivered flat-out before pacing engages. This is the buffer a stream *starts* with |
| Then pace at | `playback.hls_readrate` | 2× | How fast the input is read afterwards. Every second of wall clock adds a second of runway at 2× |
| Buffer limit | `playback.hls_ahead_max_secs` | 180 s | How far ahead of the client the session may get before it pauses itself |
| *(no dropdown)* | `playback.hls_ahead_max_bytes` | 2 GB | The same limit in bytes, per session — 180 s is a few hundred megabytes at a transcode rung and over a gigabyte of 4K copy, so time alone is not a disk bound |
| *(no dropdown)* | `playback.hls_scratch_max_bytes` | 8 GB | Ceiling across *every* live session. A per-session cap bounds one runaway; it says nothing about four healthy 4K streams between them |
| *(no dropdown)* | `transcode.max_hw_sessions` | 2 | Concurrent transcodes on the hardware encoder. An iGPU has one video-processing block, and a third 4K session on it does not run a third as fast — it drags all three under realtime. `0` disables hardware transcoding entirely (useful when the GPU is doing something else). Raise it on a card with more than one encode chip |

The last two have no dropdown because they are safety limits rather than
preferences — the right value is a property of the disk, not a taste — but
both are settable through `PUT /api/v1/settings` when the defaults don't suit
the hardware.

**How to read it:** the head start is the single number that decides whether a
4K stream survives a network hiccup ten seconds in. Until 2026-07-28 the copy
path ran at exactly real time with no head start at all, which is why 4K
"started fine and then buffered", and why an Apple TV — which wants about three
segments before it plays anything — took a dozen seconds to start. If 4K still
stutters, raise the head start before touching anything else. If sessions eat
too much disk, lower the buffer limit: a session's directory holds roughly the
buffer limit plus a minute of already-played content, whatever the encoder's
speed.

"Ahead of the client" means ahead of what the player has **downloaded**,
which is not the same as ahead of what you are watching — a player fetches
its whole forward buffer in advance, so the download frontier normally sits
about a minute past the picture on screen. Everything plurx keeps and deletes
is measured from that frontier with the difference allowed for, which is why
segments survive roughly two minutes behind it rather than one.

The pause is real: at the buffer limit plurx sends the session's ffmpeg a
`SIGSTOP` and resumes it with `SIGCONT` once you are within half the limit.
`Settings → Activity` shows a held session, and the player's stats overlay
(press `i`) says `held` beside how far ahead it is. A held session is healthy —
it has built everything it is allowed to.

### Where the transcode scratch lives

Session segments are written under `<data_dir>/transcode`, which is wiped at
every start. Two consequences worth acting on:

- **Keep the data directory off the NAS.** If `PLURX_DATA_DIR` sits on a
  network mount, every segment crosses the network twice — written by ffmpeg,
  read back by the HTTP handler — on top of reading the source. Local disk for
  the data directory, network mounts for media only.
- **tmpfs is a good fit if you have the RAM.** The buffer limit bounds a
  session, so the size is predictable: roughly `(ahead limit + 60 s) ×
  bitrate`. At the 180 s default that is well under a gigabyte for a 720p
  transcode and around 1.5 GB for a 4K copy-video session. Mount
  `<data_dir>/transcode` as tmpfs and size it for the concurrent sessions you
  expect, or lower the buffer limit to fit.

For media on NFS or SMB, the head start is read from the NAS at whatever rate
the mount can serve, so a starved mount now shows up as an encode speed below
the configured pace in the stats overlay rather than as an unexplained stutter.
Larger read sizes (`rsize=1048576` on NFS, SMB3 multichannel where the NAS
offers it) help the burst land quickly.

## Pairing another application (monarr) — the runbook

Another application can ask plurx to index exactly the folder it just wrote,
instead of plurx finding it on the next scheduled sweep. Three steps.

**1. Issue a key.** Settings has no keys screen yet (it lands with the rest
of this work), so today it is one call with your admin token:

```bash
curl -sX POST http://plurx:32400/api/v1/keys \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name":"monarr","scopes":["scan:trigger","status:read"]}'
```

The response carries `key_secret` — `plx_…` — **once**. Copy it now; it is
not stored in a form anyone can read back, including you. Losing it means
issuing a new key, which is the correct cost of never storing it.

**2. Check what it can do.** `scan:trigger` asks for scans; `status:read`
reads the result of one. A key holds exactly what you gave it: it cannot read
`/api/v1/settings` (where the TMDB and Trakt secrets live), manage users,
stream media, or mint another key. That is the entire reason keys exist
rather than "give it an admin account" — see [SECURITY.md](SECURITY.md).

**3. Point the other application at it** — plurx's URL and the `plx_` secret.

### What a scan request looks like

```bash
curl -sX POST http://plurx:32400/api/v1/scan \
  -H "Authorization: Bearer plx_…" -H 'content-type: application/json' \
  -d '{"path":"/media/movies/Heat (1995)","ids":{"tmdb":949},
       "correlation_id":"t-42-a3f9c1","source":"monarr"}'
```

**How to read the answer:**

- `200 {"status":"scanned", "report":…, "items":[{item_id,file_id,path}]}` —
  it ran now. `items` is what the path became; `report` counts what changed.
- `202 {"status":"queued","request_id":"sr-…"}` — that library was already
  scanning, so the request is queued rather than dropped. Poll
  `GET /api/v1/scan/requests/sr-…`. This is normal, not a warning: a season
  import fires one request per episode within seconds.
- `422 {"error":"path is not under any library root","roots":[…]}` — **the
  common one.** The path plurx was handed is not inside any library it knows
  about, and the body lists the roots it checked. Nearly always a container
  path-mapping mismatch: the other application says `/data/media/...` and
  plurx has that same folder mounted at `/media/...`. Fix the mapping on the
  sending side; nothing is wrong on plurx's.
- `401` — no key, an unknown key, a revoked key, or a *user token* (user
  tokens do not open this route by design). `403` — a real key without the
  scope.

`correlation_id` is echoed back, recorded on the request, and logged under
the `plurxd::integrate` target, so one `grep t-42-a3f9c1` across both
applications' logs reconstructs the whole transfer.

**Where to look afterwards.** Settings → the Server card carries
`scan_requests`: the recent asks, what they were for, and what came of them.
If it is empty, the other application has never successfully reached plurx —
check its URL and key before looking at anything else.

**What a targeted scan will never do:** remove anything. It indexes what it
finds under the path and touches nothing else, because it only saw one
folder — pruning against that view would delete the rest of the library. The
scheduled scan (per library, Settings → interval) remains the thing that
notices files that vanished.

**What the `ids` do.** They are not decoration. An item that arrives carrying
a `tmdb` (or `imdb`) id is enriched **by that id** — plurx fetches the detail
record directly and never runs a title search. That matters because the title
search is the step that can be wrong: `Heat (1995) Directors Cut Remux` is a
folder name, and a search for it can land on a different film. A wrong match
does not stay local either — Trakt sync matches on TMDB id, so it propagates.
If the sending application knows the id, sending it is strictly better than
not. Items sent without ids are matched by title, exactly as a manual scan
would do.

An item can also arrive with an id *before* it has any metadata, and that is
normal: the id says what it is, and enrichment then fills in title, overview
and artwork on the same pass. An item that keeps its filename as its title
means enrichment has no TMDB key configured — the scan itself succeeded.

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
| 4K HDR is slow but plays | The tone-map is running on the CPU. `GET /api/v1/system` → `tone_map` names the graph in use and why each candidate was rejected — no GPU device, a driver that refused the filter, output that didn't match the reference, or a graph that wasn't faster than the CPU chain | A rejection naming a missing device is usually a container passthrough (`--device /dev/dri`) or a missing driver package. HLG and Dolby Vision always use the CPU chain by design |
| "All hardware transcode slots are in use" | The cap (`transcode.max_hw_sessions`, default 2) is doing its job. A start waits up to 5 s for a slot, then runs in software *only if this server has measured that class of stream above realtime there* — never because the output is small, since the decode and the tone-map happen at source resolution whatever size you ask for | `GET /api/v1/system` → `hw_slots_in_use` / `hw_slots_max`. Refused at 0 of 2 is a bug; refused at 2 of 2 is the design. A 4K HDR source is the shape software cannot carry, so it is refused rather than started to stall |
| A GPU tone-map worked and then stopped | The pipeline downgrades once, per session, to the CPU chain and logs it | Look for `pipeline=` on the session's ffmpeg log line: it names what actually ran, not what the box can do |
| 4K HDR / Dolby Vision won't play | Heavy HEVC is hardware-decoded (Intel too); if the GPU can't decode it and software can't either, the session now fails fast with a clear log line instead of hanging gray | Read `plurxd::transcode` — the last ffmpeg line names the real cause (decode vs tone-map). DV profile 5 is the hardest case |
| Playback is software when you set `qsv` | The QSV probe was rejected at startup | Read `plurxd::transcode` logs; usually a driver/`/dev/dri` gap |
| No posters, just filenames | No TMDB key (movies/TV) | Add a key in Settings → Metadata (anime needs none) |
| Playing or seeking knocks a Wi-Fi client off the network (loses its IP and can't get another) | An unpaced stream is taking the whole link, starving the client's DHCP renewal of airtime | Lower **Settings → Playback → Delivery speed** to 2×; confirm with a `ping` to the gateway during a seek |
| 4K starts, then buffers a few seconds in | The session never built a head start — the classic cause was realtime pacing on the copy-video path | Raise **Settings → Playback → Transcode buffering → Head start**; check the stats overlay's Server block for the encode speed |
| Stutters every 20–40 seconds through a whole film | The encoder cannot keep up: the head start drains at (1 − speed) per second played | Stats overlay (`i`) → Server → encode speed. Below 1× means transcode, not network — pick a lower quality, or check that hardware encoding validated at startup |
| The transcoder seems to stop partway through | It reached the buffer limit and suspended itself | Expected. `Settings → Activity` marks it held; it resumes when the playhead catches up |
