# Deploying plurx

plurx is a single static binary (`plurxd`) plus an embedded web app. Pick the
path that matches your setup.

## Docker / Compose (recommended for homelabs)

Host-specific bits (media mounts, GPU, ports) live in an untracked override
file, so pulling updates never conflicts with your local edits:

```sh
cd deploy
cp docker-compose.override.example.yml docker-compose.override.yml
$EDITOR docker-compose.override.yml   # your media mounts (host:container:ro), your GPU
docker compose up -d --build          # builds the image from source the first time
```

Open `http://<host>:32600` and create your admin account. Library paths in
the web UI are the *container-side* paths (e.g. `/media/movies`). For
hardware transcode, uncomment the GPU block in your override (Intel/AMD via
`/dev/dri`, NVIDIA via the container toolkit). If another service (a
still-running Plex) owns UDP 32414, set `PLURX_GDM_PORT` in `.env`
(see `.env.example`).

## Bare metal

```sh
# Linux amd64/arm64, macOS, Windows — one binary, no runtime deps but ffmpeg.
plurxd run          # serves :32600; config via ./plurx.toml or PLURX_* env
```

Install `ffmpeg`/`ffprobe` (or point `PLURX_FFMPEG`/`PLURX_FFPROBE` at a build
such as jellyfin-ffmpeg for the best hardware/tone-mapping support). A sample
systemd unit lives in [`plurxd.service`](plurxd.service).

### Hardware transcode & recent Intel GPUs

The Docker image defaults to **jellyfin-ffmpeg**, which bundles a current Intel
media driver + libva + oneVPL. This matters for newer silicon: an Arc / Meteor
Lake / **Arrow Lake** iGPU (on the kernel `xe` driver) is years newer than the
VA driver Debian ships, so the distro ffmpeg fails VAAPI init with an I/O error
while jellyfin-ffmpeg drives it fine. Pass the GPU through and add the render
group in your compose override:

```yaml
    devices:
      - /dev/dri:/dev/dri
    group_add:
      - "992"          # `stat -c '%g' /dev/dri/renderD128` on the host
```

On Intel **Arc**-class GPUs, QuickSync (oneVPL) is usually more reliable than
VA-API — set `PLURX_HWACCEL: "qsv"` in the override to prefer it. Startup
validation test-encodes each path and Settings → Logs shows why any hardware
probe was rejected.

## Unraid

Add [`unraid-plurx.xml`](unraid-plurx.xml) as a user template (or via the
Docker "Add Container" screen), set your media and appdata paths, and
optionally pass through `/dev/dri` for QuickSync/VA-API.

## TrueNAS SCALE / Kubernetes

Use the Docker image with a `hostPath`/PVC for `/var/lib/plurx` and a
read-only mount for media. A Helm chart with the 3-node HA StatefulSet lands
in Phase 4 (see [../docs/ROADMAP.md](../docs/ROADMAP.md)); until then run a
single replica.

## Ports

| Port | Proto | Purpose |
|---|---|---|
| 32600 | TCP | HTTP API + web app (and the Plex-compat façade) |
| 32414 | UDP | GDM discovery so Plex/Kodi clients find the server on the LAN (host port movable via `PLURX_GDM_PORT`, but discovery only works on 32414) |

## Observability

`GET /healthz` (liveness), `GET /readyz` (storage reachable), and
`GET /metrics` (Prometheus text: uptime, active transcode sessions, library
and user counts).
