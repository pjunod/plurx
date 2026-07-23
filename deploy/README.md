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
such as jellyfin-ffmpeg for the best hardware/tone-mapping support). To keep it
running across reboots, install it as a service — **systemd** on Linux or
**launchd** on macOS, both below.

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

## Run as a service — systemd (Linux)

Keeps plurxd running across reboots and restarts it if it crashes. The unit
([`plurxd.service`](plurxd.service)) runs as a dedicated unprivileged `plurx`
user and is sandboxed (`ProtectSystem=strict`, `NoNewPrivileges`), writing only
to its data dir.

```sh
# 1. Install the binary + a service user + its data dir
sudo install -m755 plurxd /usr/local/bin/plurxd
sudo useradd --system --home /var/lib/plurx --shell /usr/sbin/nologin plurx
sudo install -d -o plurx -g plurx /var/lib/plurx

# 2. Install the unit and start it (edit paths/env in the file first if needed)
sudo cp deploy/plurxd.service /etc/systemd/system/plurxd.service
sudo systemctl daemon-reload
sudo systemctl enable --now plurxd

# 3. Watch it come up
systemctl status plurxd
journalctl -u plurxd -f            # live logs; Ctrl-C to stop tailing
```

Open `http://<host>:32600` and create your admin account. To update later,
replace the binary and `sudo systemctl restart plurxd`.

- **Hardware transcode:** uncomment `SupplementaryGroups=render` in the unit
  (match `stat -c '%G' /dev/dri/renderD128`, usually `render`) so the `plurx`
  user can reach the GPU. The software x264 path works without it.
- **Media under `/home`:** the unit sets `ProtectHome=true`, which *hides*
  `/home` from the service — if your library lives there the scan finds nothing.
  Add a `ReadOnlyPaths=/path/to/media` line, or set `ProtectHome=read-only`.
- **ffmpeg:** uncomment the `PLURX_FFMPEG` line to use a jellyfin-ffmpeg build
  for the best hardware/tone-mapping support.

## Run as a service — launchd (macOS)

Runs plurxd as a **LaunchAgent** in your login session — start-at-login, restart
on crash. A user agent rather than a boot-time system daemon on purpose:
VideoToolbox hardware transcoding needs a logged-in GUI session, which a daemon
doesn't have. The template is [`com.plurx.plurxd.plist`](com.plurx.plurxd.plist);
launchd doesn't expand `~`, so the install fills in absolute paths for you.

```sh
# 1. Install the binary + ffmpeg (Homebrew satisfies the runtime dep)
brew install ffmpeg
sudo install -m755 plurxd /usr/local/bin/plurxd     # /opt/homebrew/bin on Apple Silicon

# 2. Fill in your username + real binary paths, drop the agent into place
mkdir -p ~/Library/LaunchAgents "$HOME/Library/Application Support/plurx"
sed -e "s|YOUR_USERNAME|$USER|g" \
    -e "s|/usr/local/bin/plurxd|$(command -v plurxd)|" \
    -e "s|/usr/local/bin/ffmpeg|$(command -v ffmpeg)|" \
    -e "s|/usr/local/bin/ffprobe|$(command -v ffprobe)|" \
    deploy/com.plurx.plurxd.plist > ~/Library/LaunchAgents/com.plurx.plurxd.plist

# 3. Load, enable, and start it (modern launchctl)
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.plurx.plurxd.plist
launchctl enable   gui/$(id -u)/com.plurx.plurxd
launchctl kickstart -k gui/$(id -u)/com.plurx.plurxd

# 4. Check it, then open the app
launchctl print gui/$(id -u)/com.plurx.plurxd | grep -E 'state|pid'
tail -f ~/Library/Logs/plurxd.log
open http://localhost:32600
```

macOS prompts once to allow incoming connections (needed for other devices to
reach `:32600`). To update later, replace the binary and re-run the `kickstart`
line. To stop and remove it:

```sh
launchctl bootout gui/$(id -u)/com.plurx.plurxd
rm ~/Library/LaunchAgents/com.plurx.plurxd.plist
```

For a headless Mac that must run **with no one logged in**, install the same
plist as a system **LaunchDaemon** in `/Library/LaunchDaemons/` (owned by
`root`; add a `<key>UserName</key>` for a non-root account) and load it under
`system/` instead of `gui/$(id -u)`. The trade-off is real: no GUI session means
no VideoToolbox, so hardware transcoding falls back to software x264.

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
