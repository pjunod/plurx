# Deploying plurx

plurx is a single static binary (`plurxd`) plus an embedded web app. Pick the
path that matches your setup.

## Docker / Compose (recommended for homelabs)

Host-specific bits (media mounts, GPU, and shared Docker networks) live in an
untracked override file, so pulling updates never conflicts with local edits:

```sh
cd deploy
cp docker-compose.override.example.yml docker-compose.override.yml
$EDITOR docker-compose.override.yml   # your media mounts (host:container:ro), your GPU
cd .. && make docker-up                  # builds from source; stamps the commit into the build
```

Open `http://<host>:32400` and create your admin account. Library paths in
the web UI are the *container-side* paths (e.g. `/media/movies`). For
hardware transcode, uncomment the GPU block in your override (Intel/AMD via
`/dev/dri`, NVIDIA via the container toolkit). If another service (a
still-running Plex) owns UDP 32414, set `PLURX_GDM_PORT` in `.env`
(see `.env.example`).

The base Compose stack keeps `plurxd` on ordinary Docker networking and
publishes TCP 32400 plus UDP 32414. Your override may therefore attach it to an
external network such as `media`. A separate `plurx-discovery` companion uses
host networking only for Bonjour `_plurx._tcp`; it reads the server identity
through published port 32400 and advertises the host's LAN address. That split
keeps automatic iPhone, iPad, Apple TV, and Android discovery without moving
the media server off the networks its peer services use.

When `PLURX_SERVER_NAME` is still the default `plurx`, the companion advertises
the Docker host name plus its LAN address, so a picker says
`m6 · 192.168.1.20` instead of showing another anonymous `plurx` row. Set a
custom `PLURX_SERVER_NAME` when a room or role name is clearer; the custom name
replaces the host name while the address remains visible.

Do not add `network_mode: host` to `plurxd`. Compose forbids one service from
declaring both host networking and `networks`, so doing that recreates the
configuration error the companion is designed to avoid. If the Docker host
cannot provide host networking, the server still works at
`http://<host>:32400`, but native clients must use manual entry.

### Project name

The compose file pins `name: plurx`. Left to itself, Compose names the project
after the directory holding the file — which here would be `deploy`, a name a
great many other self-hosted stacks also use. Two stacks that resolve to the
same project name *are* the same project as far as Compose is concerned:
`docker compose ps` in one lists the other's containers, and `up
--remove-orphans` or `down` in one **deletes** the other's, on the reasoning
that they aren't in the compose file it just read. The container disappears
outright rather than exiting, so there is no exit code and no log left to read.

Don't remove the `name:` line, and if you run several stacks from directories
called `deploy`, give each of them one too.

### The data directory

Everything that must survive a rebuild — the database, artwork cache and
transcode scratch, which is to say every user, library, watch position and API
key — lives in one directory, bind-mounted from the host:

```yaml
- ${PLURX_DATA:-/srv/plurx}:/var/lib/plurx
```

Create it before the first run, owned by the uid the container runs as:

```sh
sudo install -d -o $(id -u) -g $(id -g) /srv/plurx
```

**Why a bind mount and not a named volume.** A named volume lives at a path
Docker chose, and pointing a container at one that does not exist yet is not an
error — Docker creates an empty one, plurxd initialises a fresh database in it,
and the first-run setup screen appears. That is indistinguishable from losing
your library, and it happens for undramatic reasons: renaming the deployment
directory, recreating the stack in a way that drops the volume, a `down -v`
typed in the wrong terminal. A path you can `ls` cannot go missing quietly.

**Moving from the old named volume.** Revisions before this used
`plurx-data:/var/lib/plurx` (and older ones `<project>_plurx-data`, usually
`deploy_plurx-data`). Copy it across once, with the stack down so you are not
copying a live database:

```sh
docker compose down
docker volume ls | grep plurx-data                  # confirm the source name
sudo install -d -o $(id -u) -g $(id -g) /srv/plurx
docker run --rm -v plurx-data:/from -v /srv/plurx:/to \
    alpine sh -c 'cd /from && cp -a . /to'
sudo chown -R $(id -u):$(id -g) /srv/plurx
docker compose up -d
```

Keep the old volume until you have confirmed your users and libraries are
intact, then `docker volume rm plurx-data`.

**Checking what is actually mounted.** One command, worth running whenever
something that should have persisted did not:

```sh
docker inspect -f '{{range .Mounts}}{{.Type}} {{.Source}} -> {{.Destination}}{{"\n"}}{{end}}' plurxd
```

Nothing mapping to `/var/lib/plurx` means the next rebuild loses everything.

## Bare metal

```sh
# Linux amd64/arm64, macOS, Windows — one binary, no runtime deps but ffmpeg.
plurxd run          # serves :32400; config via ./plurx.toml or PLURX_* env
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

Open `http://<host>:32400` and create your admin account. To update later,
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
open http://localhost:32400
```

macOS prompts once to allow incoming connections (needed for other devices to
reach `:32400`). To update later, replace the binary and re-run the `kickstart`
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
optionally pass through `/dev/dri` for QuickSync/VA-API. The template uses
host networking because Bonjour multicast does not leave a Docker bridge.
That is what makes `_plurx._tcp` visible to iPhone, iPad, and Apple TV; a
bridge deployment can still use manual server entry, but cannot provide
automatic native discovery.

Host networking also means the ports are real host ports. Stop any process
already using TCP 32400, or change `PLURX_BIND`. If Plex still owns UDP 32414,
change `PLURX_GDM_PORT`; this disables GDM discovery on its fixed standard port
but does not affect Bonjour.

## TrueNAS SCALE / Kubernetes

Use the Docker image with a `hostPath`/PVC for `/var/lib/plurx` and a
read-only mount for media. A Helm chart with the 3-node HA StatefulSet lands
in Phase 4 (see [../docs/ROADMAP.md](../docs/ROADMAP.md)); until then run a
single replica.

## Ports

| Port | Proto | Purpose |
|---|---|---|
| 32400 | TCP | HTTP API + web app (and the Plex-compat façade) |
| 32414 | UDP | GDM discovery so Plex/Kodi clients find the server on the LAN (host port movable via `PLURX_GDM_PORT`, but discovery only works on 32414) |
| 5353 | UDP multicast | Bonjour `_plurx._tcp` discovery for native clients; the Compose companion owns this on the host network |

## Observability

`GET /healthz` (liveness), `GET /readyz` (storage reachable), and
`GET /metrics` (Prometheus text: uptime, active transcode sessions, library
and user counts).
