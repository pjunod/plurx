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

Audited 2026-08-15. Where a claim below is exhaustive, the test that keeps it
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

## Network priors — credential generations, never reusable user ids

Node-local network priors are keyed by a lowercase SHA-256 credential-generation
digest, not by the reusable numeric user id. The digest uses the
`plurx/network-prior-user/v1` domain and length-delimited `user.id`,
`user.created_at`, and complete Argon2 PHC `password_hash` inputs. Authentication
captures it once and carries it internally through lookup and observation, so an
in-flight event cannot switch identities after a reset or delete/recreate.

Password reset, password rehash, and delete/recreate start cold; an admin-role
change preserves the generation. The digest is excluded from APIs, logs,
metrics, telemetry payloads, and client diagnostics. A non-lookup numeric user
id is retained beside each node-local row only to enforce the 64-network bound
across credential generations; lookups remain scoped exclusively by the digest.
SQLite v22 and the next
node-local Hiqlite sidecar migration drop old numeric-key rows because they
cannot be translated safely. This correction adds no replicated migration:
replicated schema remains v6 and protocol remains v4.

## Offline media leases — one package, one narrow capability

The signed-in JSON API owns package creation, status, lease issue, and
deletion. Once a package is ready, the phone generates a 256-bit random lease
token and presents it over the authenticated package route. The server stores
only its SHA-256 hash. A lease opens only that package's HLS manifest,
segments, and selected subtitle; it cannot browse the library, call a JSON
API, read another title, or mint another lease.

Native background download frameworks cannot reliably attach the account
header to every nested HLS request, so the capability lives in the media path:

```text
/api/v1/offline/media/<lease>/master.m3u8
```

The path is a bearer credential. It has a rolling seven-day expiry; successful
media reads keep the same stable value alive, with renewal writes persisted at
most once per minute so segment fetches cannot become a database write storm.
A package accepts only one value so a retry cannot silently rotate a transfer
in progress. Package deletion or expiry removes the server-side capability.
HTTP trace spans omit all query strings and replace the offline lease and live
HLS session segments with `[REDACTED]`.

Offline viewing is **not DRM**. Media on the phone is protected by the
platform application sandbox and device-at-rest controls, is kept out of
ordinary cloud backups, and is not exported to shared storage. Revoking a
user or login token prevents new server access but cannot erase bytes already
downloaded to a device. Uninstalling the app removes those local downloads;
a rooted or jailbroken device can read them.

## EPUB publication sessions — one revision, no account bearer

An EPUB is hostile archive and markup input, not trusted web UI. The signed-in
client opens it through `POST /api/v1/files/<id>/publication`; that request
checks the authenticated book request and exact file size/mtime, parses only
bounded OCF, package, and navigation metadata, then returns a random two-hour
sliding session. Publication markup receives URLs under:

```text
/api/v1/publication/<session>/<normalized resource>
```

The session can read only resources declared by that one EPUB manifest. It
cannot browse the library or call authenticated JSON routes, and a changed
source revision invalidates resource reads even before the session expires.
The owner may close it early; node-local storage is capped at 64 live
sessions. HTTP traces redact the session path just as they redact HLS and
offline-media capabilities.

Archive validation rejects unsafe or duplicate names, an escaping relative
link, encryption, more than 20,000 entries, more than 1 GiB declared expanded
data, a ratio above 1,000:1, package/navigation markup above 8 MiB, and any
served child above 128 MiB. Resource responses use `nosniff`, no referrer,
same-origin resource policy, and a CSP that blocks scripts, connections,
forms, nested frames, objects, base changes, and non-local images/fonts/media.
Child resources decompress through a two-chunk, 64 KiB channel, with no more
than eight readers active per node, so concurrent requests cannot turn the
per-resource ceiling into concurrent whole-file allocations.

The web reader supplies the other half. Its iframe grants only
`allow-same-origin`: trusted Cinema code needs DOM inspection for pagination
and locators, but publication markup gets no script, form, popup, download, or
top-navigation grant. The resource CSP independently denies scripts and
connections. A deterministic hostile-EPUB browser test proves inline script
and event handlers stay inert, remote style/image/form targets make no probe
request, and the account bearer never appears in a publication URL. The two
locks are intentional—neither the sandbox nor the headers substitute for the
other.

Native phone/tablet readers add a second trusted-container boundary without
weakening that publication sandbox. Their `WKWebView`/Android `WebView` loads
`?native-reader=1` at the configured Cinema origin, then receives the account
bearer through one direct native-to-JavaScript call. The token is absent from
the URL and web storage. Native navigation accepts only the same origin's root,
bundled assets, and publication resources; popups, file/content access, mixed
content, and unsafe browsing are refused. The JavaScript bridge is outbound
only and accepts a small allowlist of lifecycle events—publication code has no
native method that returns credentials. Dismissal destroys the reader,
releases its publication session, and clears the JavaScript authority.

Apple's offline EPUB surface removes network authority entirely. It stores the
authenticated original and server-bounded declared resources under the
profile/revision catalogue, atomically exposes the completed directory, and
serves it only through `cinema-book://offline/`. A `WKContentRuleList` blocks
all HTTP(S) resource loads before publisher CSS, images, or markup can reach a
network; the iframe still omits script, forms, popups, downloads, and top
navigation. The offline WebView receives a manifest, locator, and presentation
preferences, never an account bearer.

Android's offline surface enforces the same authority boundary with a
synthetic `https://offline.cinema.invalid` origin. `WebViewClient` intercepts
every permitted shell and publication request and resolves it from packaged
assets or the canonical app-private publication root; everything else receives
a local denial and never reaches DNS. Publisher paths are manifest-declared,
bounded per resource and in aggregate, and limited to two concurrent local
streams. The WebView has file/content access, mixed content, popups, DOM
storage, and redirects disabled. Capability downloads refuse redirects and
same-origin escapes, and the offline bridge receives no bearer or credential
getter.

## Cluster voter disks — the Trakt credential is encrypted at rest

Passwords, login tokens, API keys, and offline lease capabilities are stored
as hashes, because plurx only ever needs to *verify* them. Trakt is the
exception: plurx must **replay** the upstream OAuth access and refresh tokens
to make outbound calls, so a hash is not an option. Stored in the clear, those
two columns would be copied into every voter database, raft WAL, snapshot, and
backup once M2 selects the replicated backend — and deleting the row afterwards
cannot erase the historical log entries.

So the bearer pair is **envelope-encrypted before it reaches the `Store`**.
`TraktAuth.access_token` and `.refresh_token` are `SealedSecret`, not `String`,
and outside `plurx-core` the only way to obtain one is to seal with the key —
so a caller cannot hand a store a string it invented. Inside the crate the type
alone is not the guarantee, because a pre-encryption row must still be readable
to be upgraded; there, the **durable write itself refuses** an unsealed value.
Raft carries ciphertext, key id, expiry, username, and sync metadata. A copied
voter disk is not enough to use a household's Trakt account.

| Property | How |
|---|---|
| Algorithm | XChaCha20-Poly1305, 24-byte random nonce per seal. Two seals of the same token differ, and voters need no nonce coordination |
| Envelope | `plxenc:v1:<key id>:<hex nonce‖ciphertext‖tag>` — the key id says which key opens it, and is a one-way function of that key |
| Binding | The row's `user_id` is authenticated additional data. Repointing a sealed row at another `user_id` makes it fail to open rather than handing one household member another's account |
| Key at rest | A 32-byte key in a mode-`0600` file beside `node.id` — `<data_dir>/credentials.key`, or `cluster.credential_key_file` / `PLURX_CREDENTIAL_KEY_FILE`. Node-local configuration, never a durable row, never replicated, and never returned by a settings or status API. Cluster admission is the one narrower transport exception: the admin-only token-issuance response wraps the key inside the opaque encrypted one-time token described below |
| Unwrapping | Only at the point of an outbound call, via `TraktAuth::reveal_access_token`. The cleartext is a `Secret`: `Debug`/`Display` redact, there is no `Serialize`, and it is zeroized on drop |
| Upgrade | A boot with cleartext rows seals them in place and logs the row count and key id — never the credential. Columns are sealed one at a time, so a boot killed mid-migration is finished by the next one instead of double-wrapping what already made it |
| Durable write | **Refusal.** Every backend funnels its bearer columns through one check that the value really is an envelope, so "a durable Trakt row holds ciphertext" is enforced at the write rather than assumed of each caller |
| SQLite→Hiqlite import | **Refusal.** Import is the one production path that does not go through a durable writer — it copies source columns straight into the target — so it audits the backup's credential columns *before* submitting any row, and refuses a backup whose bearer pair is not a parseable envelope. Checked up front because a half-imported database can be discarded and a committed raft entry on three voters cannot. The node-local key is not part of this boundary, so authenticated decryption remains the outbound-call check; startup separately refuses envelopes that name a different key. The remedy is the upgrade path: boot this build on the SQLite install so it seals the rows, then take a fresh backup. A structurally damaged envelope is refused with the cleartext, since replicating a credential nobody can open makes the source install's problem permanent rather than fixing it |
| Missing key | **Refusal.** A key file that is absent while sealed rows exist stops startup with the path it wants. Minting a replacement would lock the household out of Trakt with no error to search for; reading the rows as cleartext would undo the encryption. There is no third path — `open_trakt` returns an error for an unwrapped value, it never returns the value |
| Wrong key | **Refusal.** Startup compares the key ids the sealed rows name against the key it loaded, and stops naming both when they disagree — a database restored beside a replaced key file does not boot. A key that opens nothing is the missing-key failure in a disguise: it would reach `listening`, fail every Trakt call as "not linked", and leave no error to search for. Key ids are one-way functions of their keys, so the message names which key is wanted without narrowing a search for it. A row too damaged to name any key id is one bad row, not a verdict on the key file, so it fails at use instead of stopping the server |
| Key file mode | **Refusal.** A key file that is group- or world-readable is rejected on every load with `chmod 600` as the fix — including a key restored from a backup, copied to a second voter, or written by hand, which are exactly the ones that arrive mode `0644`. A key anyone on the box can read is not the node-local key this table promises |

Kept honest by `plurx-core cluster::tests` —
`upgrading_an_install_seals_its_cleartext_trakt_row`,
`a_later_boot_leaves_an_already_sealed_row_byte_for_byte_alone`,
`a_missing_key_refuses_to_start_instead_of_falling_back_to_cleartext`,
`a_wrong_but_present_key_refuses_to_start_instead_of_opening_nothing`, and
`a_group_or_world_readable_key_file_refuses_to_start` — by the `secrets` unit
tests covering tamper, wrong key, cross-user reuse, and key-file permissions, by
`a_store_call_cannot_persist_an_unsealed_credential`, by the shared
`store_contract` assertion that neither backend's durable row holds a cleartext
bearer credential, and by `store_contract`'s
`a_cleartext_trakt_row_is_refused_before_any_row_reaches_raft`, which imports a
legacy backup through three real voters and asserts both that it is refused and
that no table was committed before the refusal.

Two limits stated plainly. The key sits beside the database, so this protects a
copied voter disk, a snapshot, and a backup — not an attacker who already has
read access to the whole data directory as the plurx user. And every voter that
must open a credential needs the same key. The first voter creates it; an M3a
join token carries it with `secret_raft` and `secret_api` inside one encrypted,
single-use envelope. Raft and the redemption/finalization HTTP bodies never
carry the key or that envelope.

## Cluster admission — one bearer, one voter, one use

`POST /api/v1/cluster/join-tokens` requires an admin login token and returns a
join token once. The joining node opens that token locally, then sends only its
SHA-256 digest to the narrow redeem/finalize endpoints. That digest proves
possession against the coordinator's stored digest without putting the
self-contained cluster secrets on the public HTTP wire. Requiring a user login
there would make a fresh node impossible to admit, while accepting an admin
token as a node credential would give it much broader authority than it needs.

The token is `plxjoin:v1:<key>:<nonce+ciphertext+tag>`. XChaCha20-Poly1305
authenticates and hides cluster id · assigned Raft id · bootstrap addresses ·
expiry · schema/protocol versions · activation proof · Raft/API secrets · the
credential-wrapping key. The self-contained key makes this an opaque bearer,
not protection from its holder: anyone who steals the complete token can use
its authority until redemption or expiry. Encryption prevents casual payload
disclosure and tampering; single-use state and a short lifetime limit replay.

| Boundary | Rule and reason |
|---|---|
| Issuance | Admin-only, 10 minutes by default, clamped to 60–3,600 seconds. A typo must not mint a near-permanent cluster credential |
| Replicated record | SHA-256 token digest · expiry · assigned Raft id · state · admitted node id. The token and cluster secrets never enter replicated SQL |
| Redemption | The fresh node sends the token digest, assigned Raft id, compatibility versions, node id, and peer addresses — never the full token. The stored digest changes atomically from `issued` to `redeeming` and is reserved to one generated `node.id`. Another node gets `join_token_reserved` rather than sharing authority |
| Finalization | Only after Hiqlite reports the assigned Raft id as a voter does state become `redeemed`. Later redemption gets `join_token_reused`; an expired token gets `join_token_expired` |
| Local material | Put the token in an owner-only file named by `cluster.join_token_file`, never in TOML or a command transcript. Successful finalization deletes that file; interrupted startup retains it only to resume the same staged identity. Local membership stores its digest, so an unrelated or copied token file warns and is ignored rather than bricking a healthy voter |
| Public status | Node id · Raft id · role · reachability · last-seen and the existing replication projection. It omits token material, peer addresses, media paths, and library data |

A joining data directory must be fresh: the presence of `plurx.db` refuses the
join instead of overwriting an installation. Schema and auth-protocol versions
are checked against the existing cluster before the process enters membership.
The join path is LAN-only and contacts only addresses carried by the token; it
has no discovery service or cloud dependency.

Removal is also fail-closed. It refuses the current leader, a change that would
leave fewer than two voters, and any target owning offline work. A replicated
path does not prove another node mounts the same bytes, so silently moving a
package would turn a membership operation into data loss.

## API keys — a credential for machines, not a token for robots

A login token **is a user**: it carries that user's privileges wholesale, and
an admin's token can read the TMDB, OMDb and Trakt secrets straight out of
`GET /api/v1/settings`. So the obvious way to let a neighbouring application
trigger a scan — "make it an admin account and give it a token" — also hands
that application every secret plurx holds. That is the hole API keys close.

A key is deliberately **not** a user. It has no account, no `is_admin`, no
identity to attribute a watch state to; it has a list of scopes and nothing
else, and it cannot widen itself.

| Property | How |
|---|---|
| Presentation | `Authorization: Bearer plx_<32 hex>` — the `plx_` prefix routes verification to the key table instead of the token table, so a key and a token are never tried against each other's store |
| At rest | SHA-256 of the secret, same discipline as login tokens (`api_keys.key_hash`); the plaintext is shown **once**, in the create response, and is unrecoverable afterwards |
| Scopes | `scan:trigger` · `status:read`. A key holds exactly the scopes it was created with. Unknown scopes are rejected at creation rather than stored, so a typo cannot produce a key that looks right and authorizes nothing |
| Revocation | `disabled` (or delete) takes away every scope at once, checked in one place — a revocation that depends on each call site remembering to check is not a revocation |
| Audit | Successful checks refresh `last_used_at` when it is more than 60 seconds old; the activity signal remains useful without a Raft write per request |
| Management | admin only — `POST/GET/DELETE /api/v1/keys`. A key cannot mint another key, or the narrow credential would be one request away from a wide one |

**Two credential kinds, two doors**, and the wall runs in both directions. A
login token does not open a key-scoped route: if it did, "monarr can trigger
scans" would still be satisfiable by handing over an admin token, and the
narrow credential would buy nothing. A key does not open a user route: there
is no "who" behind it, and inventing one would be worse than refusing.

Kept honest by
`plurxd http::tests::a_key_cannot_read_the_settings_that_hold_every_secret`,
which asserts a scan key gets 401/403 from `/settings`, `/users` and `/me` —
and by `only_an_admin_manages_keys`, which asserts a key cannot mint another.

## Who can reach what

Every route sits in one of three tiers. The public tier is small and holds no
user data; everything that touches a library or a file requires a signed-in
user; management and diagnostics require admin.

| Surface | Bar | What it exposes, and why it sits here |
|---|---|---|
| `/healthz` · `/readyz` | none | liveness/readiness for the proxy and orchestrators; no data |
| `/metrics` | none | Prometheus counters and gauges — version, uptime, active-stream/user/library counts, plus offline state/bytes/timing/quality/failure totals with fixed labels. It exposes no user, title, file, package, recipe, or device label; counts still reveal deployment shape, so firewall it to your scrape host if that shape is sensitive |
| `/server` | none | setup-required flag + feature flags, so the web app can pick first-run vs login before anyone has a token |
| `/manifest.webmanifest` · `/icons/*` · `/assets/hls.min.js` | none | static PWA assets |
| `/download/plurx-android.apk` | none | the client app *binary*, not user data — and a TV's Downloader/browser can't attach a token. Intentional; see [CLIENTS.md](CLIENTS.md) |
| Plex `/identity`, root capabilities | none | server discovery, so a Plex/Kodi/HA client can find plurx *before* it authenticates |
| Browse · playback (`/files/{id}/*`) · images · watch progress · `/client-log` | signed-in user | your library, your streams, your progress — a valid token maps to a user |
| Offline package JSON routes | signed-in owner | package creation, status, lease issue, and deletion are scoped to the user that created the package |
| `/offline/media/{lease}/*` | package lease | only the prepared HLS package named by the hashed, expiring capability; native background transfer cannot attach the account header to nested media reads |
| Users · library & settings mutations · `/system` · `/system/logs` · Trakt link · stop-session | admin | management and diagnostics — `AdminUser` is a token whose user carries `is_admin` |
| API key management (`/keys`) | admin | issuing a credential is a management action; using one is not (above) |
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

## Transport — public HTTP, internal cluster TLS

plurx's public HTTP/API listener remains **plain HTTP**. It does not terminate
public HTTPS, because on a home LAN the certificate story is pure friction and
the design target is "works on `http://192.168.x.x:32400` out of the box." The
Android client even sets `usesCleartextTraffic` for the same reason.

Hiqlite's Raft and internal cluster-API listeners use automatic rustls TLS and
their separate shared secrets. They may bind a reachable LAN address for voter
membership; startup refuses any public cleartext form of either listener. Those
internal listeners do not make the public API or the join redemption URL HTTPS.
Peer addresses are internal membership data and are omitted from the public
node-status payload. The automatic certificates encrypt traffic but are
self-signed and accepted without certificate verification; the shared secrets,
not a certificate authority, authenticate membership requests.

The consequence is explicit: **anything past a network you fully trust belongs
behind a TLS-terminating reverse proxy** (Caddy, nginx, Traefik). Over plain
HTTP the bearer token crosses the wire in the clear, so on an untrusted segment
a passive listener can lift it and gain exactly the holder's access. The proxy
is also where you add the two things plurx intentionally omits below —
HTTPS and request rate limiting. Deploy recipes: [deploy/README.md](../deploy/README.md).

Cluster admission narrows that exposure but does not erase it. The
redeem/finalize bodies carry only the join-token digest, never its embedded
secrets. The admin issuance response still returns the complete bearer once,
so mint it over a trusted LAN or an HTTPS reverse-proxy URL and put it directly
into the owner-only file.

## Non-goals — what plurx does not defend against

Listing these is the point, not an apology — an honest boundary is what makes
the protections above believable. plurx does **not**:

- **Terminate public TLS.** No HTTPS on the public HTTP/API listener; front it
  with a reverse proxy for any exposure beyond a trusted LAN (see above). The
  Hiqlite Raft/API listeners use internal automatic TLS, not operator-facing
  HTTPS.
- **Authenticate cluster peers with a certificate authority.** Hiqlite's
  automatic self-signed certificates are accepted without verification. The
  shared Raft/API secrets authenticate requests, but an active LAN attacker who
  can impersonate an internal listener can capture those bearer secrets. Keep
  the cluster listeners on a trusted network; this milestone does not deliver
  managed peer certificates or pinning.
- **Rate-limit login or cluster admission.** There is no per-IP throttle or
  lockout on `/auth/login`, `/api/v1/cluster/join/redeem`, or
  `/api/v1/cluster/join/finalize`. Login brute-force resistance rests on
  Argon2id's cost and 256-bit tokens; admission accepts only a 256-bit token
  digest already present in replicated state. Invalid admission attempts still
  cost one leader lookup. If either surface is reachable from an untrusted
  network, put rate limiting at the proxy — the layer that can see the client
  IP.
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
- **Cluster admission** — `cluster::membership::tests` pins opaque token
  round-trip, tamper rejection, and stable error codes; the migration test
  `daemon_join_refuses_occupied_and_expired_targets_then_resumes_finalization`
  drives the production token-file and HTTP join path; and
  `plurx-cluster-check membership` drives expiry, reuse, 1→3 voter growth,
  tombstone preservation, offline-work refusal, 3→2 removal, readable status
  during quorum loss, and quorum-loss refusal through real processes.

Per the project rule, **security-relevant behavior and this doc change in the
same commit** — a change that alters who can reach what and doesn't update this
file is incomplete. Dates here are absolute ("since 2026-07-23"), never
"recently", so a stale line is a visible one.
