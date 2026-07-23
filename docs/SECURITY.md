# Security — what plurx protects, and what it leaves to the network

> Companion to [OPERATIONS.md](OPERATIONS.md) (running it day to day) and
> [ARCHITECTURE.md](ARCHITECTURE.md) (how it's built and why) — this is the
> trust model: who can reach what, which classes of attack are handled
> in-process, and what plurx deliberately hands to the reverse proxy in front
> of it.

plurx is built for a home LAN: one binary, token auth, plain HTTP. That
premise shapes everything here. The server hardens what it owns — login,
query building, file-path resolution, subprocess arguments, and browser
output — and assumes a TLS-terminating reverse proxy owns the wire the moment
traffic leaves a network you trust. If you're about to expose the port to the
internet, read [Non-goals](#non-goals--what-plurx-does-not-defend-against)
first: several protections you'd expect at that boundary are the proxy's job,
not plurx's, by design.

Audited 2026-07-23. Where a claim below is exhaustive, the test that keeps it
honest is named inline.

## Authentication — one token bar, no anonymous back doors

Passwords are hashed with **Argon2id** (`plurx-core/src/auth.rs`), so a leaked
database is not a leaked password. Login mints an opaque **256-bit random
token**; the database stores only its **SHA-256 hash**, so a database leak
never yields a usable token either. The client sends the token back one of two
ways:

```
 request
   │
   ▼
 Authorization: Bearer <t>   ──or──   ?token=<t>      (an <img>/<video> tag
   │                                                   can't set a header, so
   │                                                   image + stream URLs
   ▼                                                   carry the token inline)
 SHA-256(t) ── lookup ──▶ tokens table ── miss ──▶ 401 Unauthorized
   │ hit
   ▼
 AuthUser(user)
   │
   ├─ handler requires admin?  ── no ──▶  run handler
   │                             yes
   ▼
 user.is_admin?  ── no ──▶  403 Forbidden
   │ yes
   ▼
 AdminUser(user) ──▶ run handler
```

Both guards live in one place — the `AuthUser` / `AdminUser` extractors in
`plurxd/src/http/extract.rs` — so a handler's argument list *is* its access
level; there is no imperative "check the token here" a handler can forget.
Login also verifies an unknown username against a **dummy Argon2 hash**
(`plurxd/src/http/auth.rs`) so a wrong-user and a wrong-password response take
the same time — a timing side channel can't enumerate who has an account.

**Why `?token=` is not a step down.** It carries the same token as the header
and is checked identically; it exists only because browsers won't attach
headers to `<img>`/`<video>` requests. Treat a URL with `?token=` as a
credential — it grants exactly what the bearer does.

## Who can reach what

Every route sits in one of three tiers. The public tier is small and holds no
user data; everything that touches a library or a file requires a signed-in
user; management and diagnostics require admin.

| Surface | Bar | What it exposes, and why it sits here |
|---|---|---|
| `/healthz` · `/readyz` | none | liveness/readiness for the proxy and orchestrators; no data |
| `/metrics` | none | Prometheus counters — version, uptime, active-stream/user/library counts. A small deployment-shape leak; firewall it to your scrape host if that shape is sensitive |
| `/server` | none | setup-required flag + feature flags, so the web app can pick first-run vs login before anyone has a token |
| `/manifest.webmanifest` · `/icons/*` · `/assets/hls.min.js` | none | static PWA assets |
| `/download/plurx-android.apk` | none | the client app *binary*, not user data — and a TV's Downloader/browser can't attach a token. Intentional; see [CLIENTS.md](CLIENTS.md) |
| Plex `/identity`, root capabilities | none | server discovery, so a Plex/Kodi/HA client can find plurx *before* it authenticates |
| Browse · playback (`/files/{id}/*`) · images · watch progress · `/client-log` | signed-in user | your library, your streams, your progress — a valid token maps to a user |
| Users · library & settings mutations · `/system` · `/system/logs` · Trakt link · stop-session | admin | management and diagnostics — `AdminUser` is a token whose user carries `is_admin` |
| Plex façade (`/library/*`, `/:/timeline`, …) | signed-in user | media bytes, metadata, and watch-state writes — requires a valid `X-Plex-Token` (a plurx token) since 2026-07-23 |

**The Plex façade is a full mirror, not a side door.** It once served a
*tokenless* request as the admin ("unclaimed LAN server" convenience), which
made `GET /library/parts/...` an unauthenticated download of any file and
`/:/timeline` an unauthenticated watch-state write. That fallback is gone:
missing or unknown token → 401, the same bar as the native API. Only discovery
(`identity`, root) stays public, because a client must reach it to learn where
to send its token. Kept honest by
`plurxd http::tests::plex_facade_requires_a_valid_token`.

## Injection surfaces — audited, with the guard named

| Class | Status | The guard |
|---|---|---|
| SQL injection | safe | every request value is a bound parameter; `format!` builds query strings from compile-time constants only |
| Path traversal | safe | request→path is `file_name()`-collapsed and exact-matched, or an allow-listed segment name, or a numeric DB id |
| Subprocess / arg injection | safe | ffmpeg/ffprobe run as argument vectors (no shell); request-derived args are numeric and range-clamped |
| Browser XSS | safe | all data into HTML is escaped; inline handlers get the two-layer `esc(JSON.stringify())` |

**SQL — parameters, never string-built values.** Request-derived values go in
as bound `?N` parameters throughout `plurx-core/src/store/sqlite/`. The
`format!`-assembled query text interpolates only constants: fixed column lists
and an `ORDER BY` fragment chosen by an `ItemSort` enum `match`, never a raw
sort string. Free-text search is tokenized on non-alphanumeric boundaries — so
tokens are alphanumeric — then passed to FTS5 as a **bound** `MATCH`
parameter, so neither SQL nor an FTS operator can break out.

**Paths — a request never names a file directly.** The only endpoint that maps
a request string onto a path is artwork, and it collapses the input with
`Path::file_name()` and rejects it unless it exactly equals the original
(`plurxd/src/http/images.rs`) — `../x`, `/etc/passwd`, and `a/b` all fail. HLS
segment names are allow-listed by `is_safe_segment` to `init.mp4` or
`seg<digits>.{ts,m4s}` (`plurxd/src/transcode.rs`), and the session directory
comes from the in-memory session map, not the URL. Everything under
`/files/{id}` and the Plex `/library/parts/...` mirror resolves its path from
the database by **numeric id** — scanner-populated, never user-supplied. Kept
honest by `plurxd transcode::tests::safe_segment_names`.

**Subprocess — vectors, not a shell, and every number clamped.** ffmpeg and
ffprobe are always spawned as argument vectors; there is no `sh -c` in the
tree, so classic shell injection has no surface. The request-derived arguments
are numeric and bounded before they reach the command: transcode height is
`clamp(144, 2160)` (`hls.rs`), start offset is `max(0.0)` formatted `{:.3}`,
the audio-track index is `max(0)` and only ever embedded mid-token as
`{input}:a:{i}?`, and a manual A/V sync offset is `clamp(-15_000, 15_000)` ms
(`stream.rs`). None can present to ffmpeg as a standalone `-option`. The binary
names themselves come from operator-set env vars, not from any request.

## The browser client — output escaping

The web app is one file that builds HTML with template strings and assigns
them to `innerHTML`, so output escaping is the whole game. `esc()`
(`plurxd/src/web/index.html`) escapes `& < > " '` — **both** quote characters,
because inline `on*` handlers are single-quoted and an unescaped apostrophe in
interpolated data would close the attribute early. Strings handed to an inline
handler get two layers, `esc(JSON.stringify(x))`: `JSON.stringify` makes a
valid JS string literal, `esc` makes it safe inside the HTML attribute. Every
free-text field the server can influence — titles, overviews, filenames,
usernames, error text, track labels — is `esc()`'d, and image URLs
interpolated into `src`/`style` are too.

**The scar.** A movie titled *Daniel Sloss: Can't* would not play in any
browser: the Play button built `onclick='play(…, "Can't", …)'`, and the raw
apostrophe closed the attribute, so the click hit an unterminated fragment and
silently did nothing. `JSON.stringify` had escaped for the JS layer but not the
HTML layer, and `esc()` did not yet cover `'`. The lesson is baked into the
rule above: a string in an inline handler needs both layers, always.

## Transport — plain HTTP, TLS is the proxy's job

plurx listens on **plain HTTP**. It bundles no TLS (the one `rustls` dependency
is for *outbound* calls to TMDB/Trakt, not the listener), because on a home LAN
the certificate story is pure friction and the design target is "works on
`http://192.168.x.x:32600` out of the box." The Android client even sets
`usesCleartextTraffic` for the same reason.

The consequence is explicit: **anything past a network you fully trust belongs
behind a TLS-terminating reverse proxy** (Caddy, nginx, Traefik). Over plain
HTTP the bearer token crosses the wire in the clear, so on an untrusted segment
a passive listener can lift it and gain exactly the holder's access. The proxy
is also where you add the two things plurx intentionally omits below —
HTTPS and request rate limiting. Deploy recipes: [deploy/README.md](../deploy/README.md).

## Non-goals — what plurx does not defend against

Listing these is the point, not an apology — an honest boundary is what makes
the protections above believable. plurx does **not**:

- **Terminate TLS.** No HTTPS in the binary; front it with a reverse proxy for
  any exposure beyond a trusted LAN (see above). This is a deployment choice,
  not a missing feature.
- **Rate-limit login.** There is no per-IP throttle or lockout on
  `/auth/login`; brute-force resistance rests on Argon2id's cost and 256-bit
  tokens. If the login is reachable from an untrusted network, put rate
  limiting at the proxy — the layer that can see the client IP.
- **Authenticate `/metrics` or the APK download.** Both are public by design
  (a scrape endpoint and a client binary). `/metrics` leaks deployment shape,
  not user data; firewall it if that matters to you.
- **Encrypt media at rest.** Files live on disk as-is; plurx assumes the host
  filesystem is the security boundary for the media itself.
- **Sandbox ffmpeg.** The transcoder runs with the server's privileges. Inputs
  are scanner-discovered files under admin-configured roots, not arbitrary
  uploads, so the exposure is the media you chose to host — but run plurx as a
  low-privilege user regardless.
- **Protect a stolen token.** Auth is a bearer token, not a cookie, so
  cross-site request forgery is not the exposure — but a token that leaks
  (shared URL with `?token=`, cleartext HTTP, a compromised client) is full
  access until it's revoked. Rotate by signing out; an admin password reset
  revokes that user's sessions.

## How this document stays honest

The trust model is enforced by tests, and the tests are named here so a reader
knows the guarantee is checked, not asserted:

- **Access tiers** — `plex_facade_requires_a_valid_token`,
  `client_log_requires_auth_and_accepts_reports`, `system_info_is_admin_only`,
  and `logs_endpoint_is_admin_only` (`plurxd/src/http/mod.rs`) assert the
  public/user/admin boundaries above hold at the router.
- **Path safety** — `safe_segment_names` (`plurxd/src/transcode.rs`) pins
  the HLS segment allow-list.
- **Credential handling** — the `auth` tests (`plurx-core/src/auth.rs`) assert
  passwords hash as Argon2id and tokens hash stably to a stored form.

Per the project rule, **security-relevant behavior and this doc change in the
same commit** — a change that alters who can reach what and doesn't update this
file is incomplete. Dates here are absolute ("since 2026-07-23"), never
"recently", so a stale line is a visible one.
