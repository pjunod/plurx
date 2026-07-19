# Deploying plurx

plurx is a single static binary (`plurxd`) plus an embedded web app. Pick the
path that matches your setup.

## Docker / Compose (recommended for homelabs)

Edit the media path in [`docker-compose.yml`](docker-compose.yml), then:

```sh
docker compose up -d
```

Open `http://<host>:32600` and create your admin account. For hardware
transcode, uncomment the GPU block for your card (Intel/AMD via `/dev/dri`,
NVIDIA via the container toolkit).

## Bare metal

```sh
# Linux amd64/arm64, macOS, Windows — one binary, no runtime deps but ffmpeg.
plurxd run          # serves :32600; config via ./plurx.toml or PLURX_* env
```

Install `ffmpeg`/`ffprobe` (or point `PLURX_FFMPEG`/`PLURX_FFPROBE` at a build
such as jellyfin-ffmpeg for the best hardware/tone-mapping support). A sample
systemd unit lives in [`plurxd.service`](plurxd.service).

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
| 32414 | UDP | GDM discovery so Plex/Kodi clients find the server on the LAN |

## Observability

`GET /healthz` (liveness), `GET /readyz` (storage reachable), and
`GET /metrics` (Prometheus text: uptime, active transcode sessions, library
and user counts).
