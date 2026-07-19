# plurx — Product Requirements

Captured from the founding interview (2026-07-19). Each section states the requirement and the decision behind it. Changes to this document are changes to the product.

## 1. Vision

A media server + client family that recreates what old Plex got right — scan a folder, get a beautiful library, press play on any screen in the house — with modern efficiency (hardware decode everywhere, hardware transcode when needed) and one genuinely new capability: high-availability clustering with shared settings and state.

**Non-goals:** streaming service integration, live TV/DVR, ads, discovery feeds, any hosted cloud component, any feature that requires an internet account to function.

## 2. Media types (v1)

| Type | In v1 | Notes |
|---|---|---|
| Movies | Yes | Editions, extras, collections |
| TV shows | Yes | Series → season → episode; on-deck / continue watching |
| Anime | Yes | First-class, not "TV with weird numbering" — see §6 |
| Music | No | Data model must not preclude adding later |
| Photos / home video | No | Same |

Anime note: the owner's library contains none *yet*; support is a deliberate bet on a popular use case. Practical consequence: anime metadata matching (§6) and the subtitle/audio data model (§3) are designed in from day one, while full styled-subtitle rendering may land as a fast-follow (see REQ-SUB-2).

## 3. Playback contract

The defining requirement: **the server gets out of the way**.

- **REQ-PLAY-1 — Direct play first.** If the device can natively play the container + video codec + audio codec + resolution/bitrate, the server serves the file bytes (HTTP range requests) and does nothing else.
- **REQ-PLAY-2 — Remux second.** If only the container (or a subtitle/audio track selection) is the problem, repackage on the fly (e.g., MKV → fMP4/HLS) without touching the video stream.
- **REQ-PLAY-3 — Transcode last.** Only when codec/resolution/bandwidth genuinely requires it, transcode using hardware encoders (QSV / VA-API / NVENC / VideoToolbox), including HDR→SDR tone mapping when the display can't do HDR.
- **REQ-PLAY-4 — Per-device capability profiles.** Playback decisions are driven by a device-profile matrix (what each client/device can direct-play), refined by client-reported capabilities. Profiles are data, not code — correctable without releases.
- **REQ-PLAY-5 — 4K HDR10 / Dolby Vision first-class.** HDR10 direct play wherever hardware allows; DV profile detection (P5/P7/P8) stored per file; sane fallbacks per device (HDR10 base layer where valid, tone-mapped SDR otherwise). No washed-out purple playback, ever.
- **REQ-PLAY-6 — Lossless audio first-class.** TrueHD/Atmos and DTS-HD MA passthrough where the device chain supports it; clean codec fallback (EAC3/AC3/AAC downmix) where it doesn't, with per-device audio policies in the same profile system.
- **REQ-SUB-1 — Baseline subtitles.** SRT/VTT (text) and PGS/VobSub (image) supported at launch; image subs burn in when the target can't render them.
- **REQ-SUB-2 — Styled subs (ASS/SSA).** Full styling — fonts, positioning, karaoke — via libass on clients that can, burn-in during transcode elsewhere. May ship as a fast-follow to v1, but track selection defaults for dual-audio anime (prefer-original + subs vs. dub) are v1 data-model requirements.
- **REQ-PLAY-7 — Trickplay/seek.** Accurate seeking in all modes (direct, remux, transcode); chapter support; resume from watch state on any client.

## 4. Users, auth, remote access

- **REQ-USER-1 — Local accounts.** Users and profiles live on the server: per-user watch state, ratings, library access permissions. Admin role for server settings. Argon2id password hashing; token-based client auth.
- **REQ-USER-2 — OIDC (optional).** Google and Apple sign-in supported as *optional* identity providers mapped onto local accounts. Never required; the server is fully functional with zero internet.
- **REQ-USER-3 — BYO remote access.** No relay service, no NAT traversal service. plurx works cleanly behind a reverse proxy (TLS termination, path/host routing), WireGuard/Tailscale, or a plain port forward. Documentation treats Caddy + Tailscale as the blessed paths.
- **REQ-USER-4 — LAN discovery.** Zero-config discovery on the local network (our own mDNS/`_plurx._tcp` + Plex GDM responder for compat clients — see §8).
- **REQ-USER-5 — Parental controls.** Content-rating restrictions per profile. (v1.x, not v1.0.)

## 5. Library & scanner

- **REQ-LIB-1 —** Point at folders; scanner identifies movies/shows/anime from filenames and structure (Plex/Jellyfin-style naming conventions honored).
- **REQ-LIB-2 —** Deep media inspection per file: container, codecs, profiles/levels, bit depth, HDR metadata (HDR10/HDR10+/DV profile), audio layouts, subtitle tracks — this feeds the playback decision engine directly.
- **REQ-LIB-3 —** Watch folders for changes (inotify + periodic rescan); incremental, resumable scans that don't hammer shared storage.
- **REQ-LIB-4 —** Multiple versions per item (4K + 1080p editions), multi-part files, extras/trailers.

## 6. Metadata

- **REQ-META-1 — TMDB** primary for movies and TV (artwork, cast, ratings).
- **REQ-META-2 — TVDB** supported for TV, especially libraries organized around TVDB IDs/ordering.
- **REQ-META-3 — AniDB/AniList** for anime: absolute episode numbering, cours/split-seasons, romaji/English/native titles, correct specials handling. An item matched as anime uses anime-correct ordering rules, not forced TVDB season shapes.
- **REQ-META-4 —** Local artwork respected (poster.jpg, fanart, theme). Provider metadata cached locally so a scanned library keeps working offline indefinitely. (.nfo sidecar support: nice-to-have, not v1.)
- **REQ-META-5 —** Manual match/fix-match UI in the web app; ID badges (tmdb/tvdb/anidb) stored per item.

## 7. High availability — the cluster contract

- **REQ-HA-1 — Topology: 1 or 3+.** Single node runs with clustering dormant (zero overhead, zero config). HA requires 3+ nodes (odd counts recommended) for raft quorum. **2-node HA is explicitly unsupported** — documented clearly, with the recommendation to add a cheap third voter (Pi/NAS-class) if desired later; no witness process in v1.
- **REQ-HA-2 — Active-active.** All nodes serve API traffic and streams concurrently against the same media on shared storage (NFS/SMB/clustered FS mounts — media storage HA is the operator's domain, not plurx's).
- **REQ-HA-3 — Replicated state.** Users, settings, library metadata, watch state, and playback-session state replicate across nodes via embedded consensus. **No external database, broker, or coordinator — ever.** Three copies of the same binary discover each other and form the cluster.
- **REQ-HA-4 — The failover promise.** A node dying mid-stream costs the viewer seconds: the client retries against a surviving node, which picks the session up from replicated state and resumes at position — including in-flight *transcode* sessions (deterministic segmenting makes any node able to regenerate any segment). Settings/watch-state writes survive any single-node loss (3-node cluster) with zero data loss once acknowledged.
- **REQ-HA-5 — Cluster UX.** Nodes share one logical identity: one server name, one settings surface, cluster health visible in the admin UI. Adding a node = install binary, paste a join token.
- **REQ-HA-6 — Client failover.** Clients receive the node list and fail over client-side; deployments may also front the cluster with a VIP (keepalived) or k8s Service/Ingress — both documented.

## 8. Plex API compatibility

Goal: existing third-party Plex clients can point at plurx and work, without plurx depending on plex.tv.

- **REQ-PLEX-1 — Tier 1 (v1 target): direct-connection clients.** Implement the PMS HTTP surface used by clients that support manual/direct server connections with no plex.tv account: **Composite for Kodi, PlexKodiConnect, python-plexapi tooling, Home Assistant.** Requires: GDM discovery responder, `/identity`, library sections/metadata browsing, image transcoding, direct part serving, `/video/:/transcode/universal/decision|start.m3u8`, timeline/scrobble progress, search, playlists; XML default + JSON via `Accept`; `X-Plex-Token` mapped to plurx tokens. Since Sept 2025 Plex publishes **official API docs** (developer.plex.tv), making this dramatically more tractable.
- **REQ-PLEX-2 — Reality check (recorded 2026-07-19): Infuse, VidHub, Symfonium, and all official Plex apps cannot connect without plex.tv sign-in.** Serving them would require emulating plex.tv itself (PIN auth, resources endpoint) plus DNS redirection on the client's network — fragile and adversarial. **Tier 2 (plex.tv emulation) is explicitly deferred**; revisit only after Tier 1 proves out. Mitigation: Apple TV users get Kodi(+Composite) day one and our native tvOS app later; plurx's own clients are the long-term answer.
- **REQ-PLEX-3 —** Compat layer is a *translation façade* over the native API — no Plex-isms leak into core data models. Never contacts plex.tv. GDM responder answers only on LAN interfaces.

## 9. Deployment targets (all first-class)

| Target | Requirement |
|---|---|
| Docker / Compose | Official multi-arch images (amd64/arm64); documented GPU/QSV device passthrough; example HA compose |
| Bare metal | Single static binary + systemd unit; Linux amd64/arm64 first, macOS/Windows server later |
| Kubernetes | Helm chart: StatefulSet (3 replicas), pod anti-affinity, device-plugin notes for GPU transcode |
| NAS packages | Unraid template + TrueNAS SCALE app first (Docker-based); Synology/QNAP native packages later |

- **REQ-OPS-1 —** Prometheus `/metrics`, structured logs, health/readiness endpoints per node + cluster.
- **REQ-OPS-2 —** Config = sane defaults + one file + env overrides; secrets never logged; settings edited in the web admin replicate cluster-wide.
- **REQ-OPS-3 —** Online upgrades: rolling node upgrades within a minor version; state migrations are forward-safe.

## 10. Client platforms

Hybrid strategy (detail in [CLIENTS.md](CLIENTS.md)): shared TypeScript web core for browser + Samsung Tizen + LG webOS; native Swift/tvOS; Kotlin + Media3 for Android/Google TV (covers Sony, Shield, phones/tablets); BrightScript/SceneGraph for Roku. **Web app ships first** and doubles as the admin UI. Third-party Plex clients (Tier 1) cover living rooms until native apps land.

## 11. Posture & constraints

- **Team:** solo + AI pair programming, steady nights-and-weekends cadence → roadmap must be small, always-shippable increments; every phase ends with something usable.
- **License:** private for now; decision deferred until sharing matters. No dependencies whose licenses would foreclose either open-sourcing or staying private (note: GPL ffmpeg is invoked as a subprocess, not linked).
- **Quality bar:** playback correctness (HDR color, audio sync, seek accuracy) outranks feature count. A small library that plays flawlessly beats a big one that stutters.
