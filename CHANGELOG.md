# Changelog

All notable changes to plurx are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and plurx uses
[semantic versioning](https://semver.org/) under the 0.x rules described in
[docs/RELEASING.md](docs/RELEASING.md): while the major number is 0, a **minor**
bump may break compatibility and a **patch** bump never does.

## [Unreleased]

### Fixed

- **Apple seeking works like a progress bar again.** Scrubbing, skip presses,
  and lock-screen position commands on iPhone, iPad, and Apple TV now seek the
  current item's own clock whenever the target sits inside the growing HLS
  playlist's advertised window — instant, with no server round trip — and only
  reopen the session for positions the transcoder has not published yet (or
  retention has pruned). Out-of-window commands coalesce for 350 ms so a burst
  of presses costs one replacement session instead of one per press. During a
  replacement, the predecessor item's failure noise (its playlist is deleted
  by supersession the moment the new create begins) no longer races the
  in-flight open into SDR/transcode fallbacks, spurious "playback stopped"
  errors, or random position jumps; a replacement that genuinely fails during
  its own attach is still caught once the change lands. The iOS slider also
  commits its seek before leaving scrub mode, so the thumb no longer flashes
  back to the pre-scrub position.

### Added

- **Performance II N0 makes playback telemetry durable and queryable.** Client
  beacons keep their existing human log lines and additionally enter a bounded,
  node-local `playback_events` table with live session speed, runway, hold,
  delivery-rate, and pacing context. The server records session lifecycle,
  suspend/resume, playlist-slide, and producer-pass events; exposes an
  admin-only reader, a seven-day System card, and bounded Prometheus playback
  series; and prunes rows daily under `telemetry.retain_days` (30 days by
  default, `0` fully disables row writes and pruning). Hiqlite voters use
  independent sidecars, never Raft, with restart and non-replication coverage.
  `scripts/perf-report` prefers the durable reader and falls back to the log
  ring on older servers. Apple build 46 emits one authenticated TTFF beacon at
  the first advancing online frame, distinguishes cold starts from resumes,
  and rebases its film-position gate at attachment and pre-frame seeks so a
  growing copy session's keyframe origin cannot inflate the result. The beacon
  includes the live HLS session id for the ingest join; offline packages remain
  silent. Android build 24 adds the native parity floor: one authenticated
  TTFF beacon per prepared attempt, timed from before the decision request;
  final-duration measurements for buffering stalls of at least six seconds
  after playback is established and while play is requested; and
  playback-error context, including the live HLS session id for the ingest
  join. Startup, paused buffering, and seek waits do not enter the stall
  series. Neither client changes playback recovery policy.

### Changed

- **CI is now two-tier: pull requests run a diff-scoped fast lane and the
  merge queue runs the full cross-surface fan-out.** Client-only diffs no
  longer pay the cargo workspace gate (Kotlin and Swift compile nothing under
  `crates/`), and server diffs defer the Apple-simulator, Android-JVM, and
  web-layout suites to `merge_group`/push events, where `all_scope()` still
  runs every surface before a commit reaches `main`. The CI Rust gate splits
  instead of shrinking: clippy is lint.yml's job, the replicated-store member
  is the `cluster_auth` job's — excluding it also stops feature unification
  from compiling the hiqlite stack into the PR gate, so the suite tests
  `plurx-core` with the features the shipped `plurxd` resolves. Waste removed
  per run: the iOS suite compiles once and iPhone + iPad replay the products;
  the Android SDK image pulls from GHCR keyed on its Dockerfile hash instead
  of rebuilding twice; Gradle, AVD-snapshot, Playwright-browser, DerivedData,
  hiqlite-spike-target, and docker-layer caches are wired; Argon2id test
  hashing is compiled optimized; and the coalescer trailing-flush test
  asserts through a bounded poll, so a saturated host cannot read the row
  ahead of the asynchronous flush it is checking. `pr_gate` reports on both
  `pull_request` and `merge_group`, so it stays the single required check
  when the queue is enabled.

### Added

- **Apple artwork now survives app relaunches.** iOS and tvOS keep original
  poster and backdrop bytes in a 256 MiB on-disk LRU, paint seven-day-fresh
  entries before touching the network, and retain stale fallbacks for up to
  30 days while a refresh is attempted. Artwork URL and server changes use
  distinct cache identities, and signing out or leaving a server clears its
  cached bytes.

- **The Phase 4 storage boundary now has a complete replicated backend and a
  real three-voter contract gate.** M0–M1c established local node identity,
  rollback-tolerant cluster configuration, the Hiqlite decision, replicated
  auth/catalogue/watch state, node-local FTS, and root-safe reconciliation.
  M1d completes all 120 `Store` methods for Trakt/outbox,
  cache, and offline packages; runs the same `Arc<dyn Store>` scenarios against
  memory SQLite, file SQLite, and three separate Hiqlite voters; and rechecks
  independently hashed state after induced follower and leader loss. Trakt
  refresh/unlink and watched delivery now use compare-and-set/claims for
  multi-writer safety, offline admission enforces per-node byte budgets and
  complete idempotency, and active progress is bounded to one durable commit
  per ten-second stream window without overwriting manual or timestamped
  writes. SQLite remains the selected production store until import,
  envelope-encrypted Trakt credentials, node-removal cleanup, and
  post-coalescer growth measurement land. Operators can explicitly accept a
  verified replacement mount through
  `POST /api/v1/libraries/{id}/root-identity/reset` and rebuild a node's
  derived search state through `POST /api/v1/system/search-index/rebuild`.

- **A default-off PGS overlay path preserves the original video.** With
  `PLURX_PGS_OVERLAY` enabled, authenticated manifest and immutable PNG routes
  let the Apple and Android players draw positioned PGS cues over unchanged
  Dolby Vision, HDR, or SDR video. Publication is bounded, durable, and
  self-healing, HD palettes are checked against a BT.709 fixture, and a pinned
  nightly SUP fuzzer keeps malformed inputs under pressure. This remains a
  staged capability, not a release claim, until the physical-device matrix
  passes.

- **Download a title in the native phone apps and watch it without the
  server.** iPhone, iPad, and Android phones/tablets now have a one-tap
  Downloads flow rather than a save-file export. plurxd durably prepares one
  portable SDR H.264/AAC HLS package with the selected audio and supported
  text subtitle, accounts for it under separate global/per-user quotas, and
  serves it through a stable seven-day rolling capability whose plaintext is
  never stored or logged. Apple hands the package to background
  `AVAssetDownloadURLSession`; Android hands it to a foreground Media3
  `DownloadService` backed by a non-evicting private cache. Both restore local
  catalogs after process death, expose saved titles before server reconnect,
  isolate them by server/user profile, play only app-owned local bytes, and
  sync the newest timestamped progress after reconnect. tvOS, Android TV, and
  Google TV intentionally retain streaming-only behavior.

- **Choosing a text subtitle on the Apple apps is no longer a video
  decision.** Selecting an SRT/SubRip/WebVTT track used to send
  `subtitle_burn`: the server opened a fresh session and started a video
  encoder to paint the text into the picture, and the player rebuilt its item
  around it. A subtitle therefore cost a re-encode, dropped Dolby Vision or
  HDR on a copied 4K remux, and discarded the quality rung the viewer had
  picked. Those tracks are now advertised in the HLS master as WebVTT
  renditions carrying their language, display name, default, forced, and
  accessibility metadata, and iOS/tvOS select them — and Off — inside the
  running player item: no new session, no encoder, no quality change, no
  ffmpeg subtitle filter. Cues are cut per segment and anchored to the
  session's own media origin rather than to the offset that was requested, so
  they stay with the picture through a resume and through a seek on a copy
  session, whose timeline starts at the keyframe before the requested point.
  Language tags come from the same alias table the rest of the server uses,
  so a track muxed `dut`, `cze`, or `gre` still matches a viewer whose
  preference is Dutch, Czech, or Greek; a track whose title says "Forced"
  counts as forced, while "Non-Forced" and "Unforced" no longer do (they were
  being hidden from Apple's subtitle menu); and a cue that straddles a
  segment boundary keeps the end time it was authored with instead of being
  clipped short in both halves. A file whose video the device can take
  directly still direct-plays — merely *containing* text tracks no longer
  forces it into a session — and entering one is deferred until a native
  subtitle is actually chosen, which costs a single reopen at that moment and
  nothing before it. Automatic selection is never allowed to start a burn on
  its own; the one exception is a forced track, which may, at source height,
  because that is what a forced track is for. PGS, VobSub, and styled ASS/SSA
  still burn and still reopen at the same film position, deliberately: they
  are positioned bitmaps and authored styling, and plain WebVTT would lose
  them.

- **And the Android app now does the same thing.** Choosing a text subtitle
  there used to send `subtitle_burn` too, which on a 4K HDR remux cost the
  stream its resolution *and* its HDR for a track the server hands over free.
  Android now takes the same WebVTT renditions the Apple apps do: on a
  direct-played file the selection happens inside the player with no server
  involved at all, and on a remuxed or transcoded one it opens a session
  whose video recipe is untouched. Switching between two text tracks changes
  nothing on the server. Bitmap and styled tracks still burn, at source
  height, and the subtitle menu no longer lists a track twice on direct play.

- **One place decides which subtitle comes on by itself.** The server already
  worked this out — it honours anime dual-audio rules, the configured
  audio/subtitle languages, and the subtitle mode (Auto, Always, Off) — and
  said so in the playback decision, but each app then re-derived the rule from
  scratch and reached its own answer. An app set up against a server whose
  subtitle mode was Auto behaved as if it were Always. Both apps now apply
  the server's choice. They keep exactly one rule of their own, and it is a
  refusal rather than a preference: an app will not start a burn on its own
  initiative for a track that is not forced, whatever the server picked,
  because a burn is a re-encode and nobody asked for one.

- **Point an app at this server by scanning a code.** The web sign-in screen
  now offers a QR code of the server's own address, and the iOS and Android
  apps read it to add a server. That is for the networks Bonjour cannot
  cross — a separate VLAN, guest Wi-Fi, a VPN, a Docker bridge — where the
  alternative was typing a LAN address with a TV remote. The code contains
  the address and nothing else: no token, no account, and you still sign in
  inside the app.

- **The apps say which build they are.** Settings on iOS/tvOS and on Android
  now shows the app's version and build number next to the server's, so a
  report from a device names the build it came from instead of "the app".

- **Android labels what a file actually is.** Detail and player pages carry
  compact chips for resolution, video codec, dynamic range, and audio, drawn
  with real icons and given their own accessibility text, so picking between
  two versions of a title stops being a guess about which one is the remux.

- **The playback decision says whether a subtitle can be served as a
  rendition.** Its `text` field only ever meant "not a bitmap", so ASS and SSA
  came back as text even though their authored styling cannot survive
  conversion to WebVTT and the server refuses to serve them as one. Each app
  was carrying its own copy of the codec list to work around it. The decision
  now carries `native` alongside `text`, computed by the same rule the HLS
  master and that refusal use, so there is one answer instead of three.
  Additive — older apps ignore it.

- **Two HLS master experiments ship compiled in and off by default.**
  `PLURX_HLS_CLOSED_CAPTIONS_NONE=1` adds `CLOSED-CAPTIONS=NONE` to the
  variant; `PLURX_HLS_FORCED_AUTOSELECT=1` puts `AUTOSELECT=YES` on forced
  renditions. Both are Apple authoring-rules items, and both are candidates
  for the one thing this work did not solve — a physical Apple TV rejecting a
  copied Dolby Vision master with CoreMedia `-12927`. They are flags rather
  than changes because every master regression so far passed the unit tests
  and failed on the device: enable one per deploy, watch the device, then
  keep it or drop it. Full description in
  [docs/OPERATIONS.md](docs/OPERATIONS.md#the-two-hls-master-experiments).
- **The library list can carry each item's media facts, in one query.** A
  browse response used to say only how tall an item's best file is; codec,
  dynamic range, audio, container and size lived on `FileDto`, which only the
  item-detail response returns — so anything wanting spec columns on a grid
  had to fetch every item. `GET /api/v1/libraries/:id/items?facts=1` now adds
  an aggregated `media` block per playable item
  (`{files, bytes, video, height, dr, audio, container}`), computed for the
  whole page in a single windowed query over `files` rather than one lookup
  per card. `files` and `bytes` cover every version of the item; the rest
  describe its best one — greatest height, then bitrate — so a 2160p Dolby
  Vision remux beside a 720p copy reads as 2160p and never as a union of
  files no single copy could play. `dr` speaks the badge vocabulary the
  clients already print (`DV`, `HDR10+`, `HDR10`, `HLG`) and states what is
  on disk, not what will be delivered. Without the parameter the response is
  byte for byte what it was, so no existing client pays for the block.
- **The activity page can see every viewer, not just the transcoding
  ones.** `GET /api/v1/activity/detail` grows a `deliveries` array beside
  the existing `sessions` — which is unchanged, in shape and in meaning —
  listing everything in flight with a `method` of `direct`, `remux`,
  `hls-copy` or `transcode`. Two of those were already tracked and simply
  never listed: an HLS copy-remux has always been a real session, and a
  progressive `/stream.mp4` remux has always been a registered stream (now
  registered whether or not the client asks for telemetry, so a native
  client that sends no stream id is still visible). Direct play had no
  record at all and now gets one, keyed by the *player* rather than the
  request — a seeking browser makes dozens of ranged requests for one film,
  and one row each would have shown a dozen phantom viewers per person. A
  direct play appears within a beacon and disappears after 30s of silence
  (three missed progress beacons), because a closed tab announces nothing.
- **Genres, server-side at last — and the backfill to fill them in.** Nothing
  in plurx has ever stored a genre for catalogue media: TMDB has returned them
  on every `/movie/{id}` call the server ever made and the field was read
  straight past, and the only `<genre>` handling in the tree folded an NFO's
  into a *home* library's free-form tags. Items now carry a `genres` list
  (schema v13, a JSON array on the row exactly like `tags`), filled at
  enrichment time and exposed as an additive `genres` field on every item the
  API returns. It costs nothing to collect: a movie's genres come out of the
  details call the match already makes, and a TV library's come from ONE
  cached `/genre/tv/list` fetch per run rather than a second round-trip per
  series. AniList supplies its own for anime libraries, free, on the search
  query that was already running. Library grids take an optional `?genre=` —
  filtered server-side, case-insensitively, with `total` filtered to match so
  paging stays honest.

  Libraries enriched before this have no genres, and nothing stored can
  produce them, so filling them in means asking the provider again once per
  title. That is an opt-in job (`genres.backfill` on the settings page), never
  something an upgrade starts on its own: it is paced well under TMDB's public
  rate ceiling, stamps its progress after every single title so a reboot or a
  rate limit resumes rather than restarts, disarms itself when it reaches the
  end of the catalogue, and reports what it backfilled, failed and skipped —
  with every failure named, because a backfill that half-finishes in silence
  is worse than one that never ran.
- **The server says which dynamic range a delivery actually carries.** Every
  client's HDR/DV badge was built from the source probe alone, because the one
  fact that would let it be honest — what grade is in the bytes this session
  sends — existed nowhere on the wire, even though the decision engine has
  always known it. `/files/{id}/decision` and the HLS session response now both
  carry `delivered_dynamic_range` (`"dolby_vision" | "hdr10" | "hlg" | "sdr"`,
  the same vocabulary as the source's `hdr` plus `"sdr"`), computed by one
  shared function so the two can never disagree: a preserved-DV copy answers
  `dolby_vision`, a stripped one answers the base layer its compatibility
  marker names, and every transcode answers `sdr` because every encoder in the
  pipeline emits H.264 8-bit through the tone-map graph. The session's answer
  overrides the decision's, since a burn or a manually-picked rung produces a
  transcode the decision never promised. Additive and nullable: nothing about
  the decision, the DV strip, or tone-mapping changed — this reports them.

- **Library grids can finally classify a show's watch state.** `/libraries/
  {id}/items` attached a `watch` row per item and nothing else, but a show has
  no watch row of its own — the state lives on its episodes, which are not in
  that response — so "Watched" and "In progress" filtered to nothing and
  "Unwatched" listed finished series. The page now carries the same `rollup`
  (`leaves` / `watched`) the detail endpoint has always returned for
  containers, batched into a single recursive query per page rather than one
  per card, so the cost does not grow with the number of shows on screen.

- **SDH subtitle renditions are tagged from the container's own flag.** The
  accessibility `CHARACTERISTICS` on a native HLS subtitle rendition was
  decided by sniffing the track's title for "SDH", "closed caption" and
  friends, so a hard-of-hearing track named "English 2" was untagged and a
  film named by someone else's convention was invisible to a viewer who needs
  captions. The scanner now reads ffprobe's `disposition.hearing_impaired` and
  the playlist prefers it, with the title sniff kept as the fallback.

- **The Apple player's dynamic-range badge, and one for the detail page at
  last.** The iOS/tvOS overlay had the same problem as the web player's: its
  HDR/DV chip was built from the source probe, so a Dolby Vision remux read
  `DV` even while a forced 1080p rung delivered tone-mapped SDR. The chip still
  starts from what the file carries and now dims and names what is actually
  arriving — `DV → HDR10` when the server strips a profile this Apple TV does
  not claim, `DV → SDR` for a transcode session or a display that reports it
  cannot show HDR — with a spelled-out VoiceOver label and a matching "Dynamic
  range" row in the playback-info panel carrying the server's own reason. It
  reads `delivered_dynamic_range` from the decision and, once a session
  attaches, from the session response, which overrides it; an absent field
  falls back to today's source-only chip. `AVPlayer.eligibleForHDRPlayback` is
  the only local signal used, asked at render time so switching an Apple TV's
  Dolby Vision output off in Settings changes the badge without a relaunch.
  Separately, the Apple **detail** page finally has a dynamic-range badge at
  all — Android and the web have had one all along — built by the same function
  so the two screens cannot label one file two ways. Nothing about the
  decision, the caps probe, or session creation changed: this reports them.

- **Apple: watch filters work on show libraries, and the first paint stops
  waiting for the last response.** "Watched" and "In progress" filtered a TV
  grid to nothing while "Unwatched" listed finished series, because a show has
  no watch row — the state lives on its episodes. Containers now classify from
  the `rollup` the server attaches to library pages. Alongside it, home fetches
  hubs, libraries, and Coming Soon together instead of in sequence, publishes
  each shelf and each library page as it arrives, and raises a spinner only over
  a screen that has never held anything — so pull-to-refresh no longer blanks a
  populated dashboard, and a thousand-item library shows its first page after
  one round trip rather than five.

- **Apple: posters decode to the cell that draws them, a failed one retries,
  and an expired token lands on the login screen.** Artwork was decoded at full
  raster into every cell — a 2000×3000 poster into a 132-point card, once per
  cell, which is what made large tvOS grids stutter — and a poster that failed
  its single fetch stayed a grey rectangle for the life of the process. Loads
  now downsample through ImageIO to the cell's pixel ceiling, off the main
  actor, into a shared `NSCache` keyed by origin and size, over a
  `waitsForConnectivity` session with one retry. Separately, a 401 or 403 on any
  request after launch now clears the bearer and returns to the login screen
  once, instead of degrading every screen to "Server returned 401" while the app
  still appeared signed in; and home refreshes when the player is dismissed and
  when the app becomes active, which tvOS never did at all.

- **The web player's dynamic-range badge says what you are getting, not just
  what the file is.** Every media badge was built from the source probe, so a
  Dolby Vision disc remux showed a full-colour "DV P7" while Chrome was in
  fact watching a tone-mapped SDR H.264 transcode of it. The play overlay's
  chip now starts from the source grade as before and, when the session is
  delivering something else, dims and names it inside the same chip — `DV P7 →
  HDR10` where the server stripped DV for this browser, `DV P7 → SDR` where it
  fell back to a transcode. The display is re-asked at render time
  (`matchMedia("(dynamic-range: high)")`), so an HDR stream on an SDR monitor
  reads `HDR10 → SDR` and moving the window between screens changes the badge.
  The stats overlay gains a matching "Dynamic range" row, built from the same
  function and carrying the server's own reason. It consumes the new
  `delivered_dynamic_range` field on `/decision` and on the HLS session
  response; an absent field degrades to the old source-only chip, and the
  detail pages stay source-only because there is no session to report on.

- **Software transcodes are admitted against a CPU budget, and spend it
  explicitly.** Software sessions used to bypass admission entirely: each
  x264 process picked its own thread count (cores × 1.5), so a few of them
  could oversubscribe every core and drag every stream on the box under
  realtime together. Admission now runs a thread-denominated pool — a
  session's weight follows the encode size plus a 4K-decode surcharge, and
  what it reserves is exactly what its encoder is told to use
  (`-threads`). The lone session on an idle box always starts, whatever its
  weight; the mid-film hardware→software fallback takes its share by force
  rather than hold a playing viewer hostage, but visibly, so later
  admissions see a full pool. The pre-transcode producer draws from the
  same pool at background priority, and *every* live start — software
  included — now signals the producer to stand down. The budget defaults to
  every core but one and is settable as `transcode.software_pool_threads`
  (see [docs/OPERATIONS.md](docs/OPERATIONS.md)).

- **`/decision` now carries an executable delivery plan, and the native
  clients execute it.** The response's new `delivery` field says what to *do*
  about the verdict — `direct { url }`, `remux { url, sessions_url, aac }`,
  `transcode { sessions_url }` — because clients that re-derived policy from
  `method` got it differently wrong on every platform. Android played every
  non-direct verdict through `stream.mp4`, which never re-encodes video, so a
  tone-map or downscale verdict shipped the copied source anyway; it now opens
  a transcode HLS session at the server's Auto rung, seeks by opening a
  session at the new offset (as the web player does), and releases sessions
  with a `DELETE`. Apple started a hardcoded 1080p re-encode for everything
  non-direct via the deprecated GET bridge; a remux verdict now becomes a
  **copy** session — a 4K HEVC/HDR MKV reaches an Apple TV at full quality
  with no encoder running, which is what [docs/CLIENTS.md](docs/CLIENTS.md)
  promised all along — and a transcode verdict names no height, because Auto
  is the server's choice. Both clients now send a stable `playback_id` (so two
  devices on one account stop killing each other's streams), a per-attempt
  `request_id`, and an explicit `DELETE` when playback ends, instead of
  leaving encoders to the idle reaper. `play_url` stays for older clients.

- **Android: the dynamic-range badge says what is on screen, not what the file
  is.** The chip was built from the source probe alone, so a Dolby Vision disc
  remux read a confident "DV" while the device was in fact watching the
  stripped HDR10 base — or, on a forced rung, a tone-mapped SDR transcode. It
  now starts from the source grade as before and, when the session is
  delivering something weaker, dims and names it inside the same chip: `DV →
  HDR10`, `HDR → SDR`. The verdict combines the server's new
  `delivered_dynamic_range` with the two signals Android actually has — the
  panel's `Display.HdrCapabilities` and the decoder's `Format.colorInfo` — and
  the decoder wins when it contradicts the plan, because it is downstream of
  every fallback. A missing decoder signal is not a contradiction, so an HLS
  variant that publishes no `ColorInfo` leaves the server's answer standing,
  and a server that does not send the field at all keeps the old source-only
  chip. The playback-info panel gains a matching "Dynamic range" row carrying
  the server's own reason. Nothing about the decision, the caps probe, or
  session creation changed: this reports them.

- **Android TV: a shelf you have scrolled is still reachable.** Each shelf's
  `FocusRequester` was attached to the card at index 0 — which a `LazyRow`
  disposes the moment the viewer scrolls past it. Every neighbouring shelf aims
  its `up`/`down` at that requester, so after one horizontal scroll the vertical
  walk was requesting focus on a requester attached to nothing. The requester
  and the direction overrides now live on the row container with `focusGroup()`,
  which exists for as long as the shelf does. Poster cards and pickers carry one
  focus target each rather than two: `clickable` is already focusable, and the
  trailing `focusable()` put an *unclickable* target in front of it, so the
  D-pad could land on a card whose centre press did nothing. The "Group by"
  picker is now a stop on the vertical chain instead of a control the shelves
  pointed straight past, which made it unreachable without a touchscreen. An
  instrumented reproduction (`ShelfFocusTest`) scrolls a shelf until its first
  card is disposed and then walks focus down and back up.

- **Android: watch filters work on show libraries, and first paint stops
  waiting for the last response.** "Watched" and "In progress" filtered a TV
  grid to nothing while "Unwatched" listed finished series, because a show has
  no watch row — the state lives on its episodes, which are not in that
  response. Containers now classify from the `rollup` the server attaches to
  library pages. Alongside it: home issues hubs, libraries, and every library
  preview at once and publishes each as it arrives, instead of 2 + N round trips
  in strict sequence; a library grid paints its first page after one round trip
  and fills in behind; sorting is client-side, so changing the sort no longer
  re-fetches the whole collection behind a spinner to receive the same items
  back; a detail page draws before the season-and-episode walk that decides what
  the Play button is *labelled*; and the spinner is raised only over a screen
  that has never held anything, so a refresh — or a refresh that fails — cannot
  blank a populated dashboard.

- **Android lifecycle edges: discovery stops, waits are bounded, and a quality
  change keeps your tracks.** Recovering a moved server started NSD discovery
  and never stopped it, leaving multicast running for the life of the process;
  it is now closed in a `finally`. `NsdManager.resolveService` is a callback
  with no timeout of its own — a service that vanished between discovery and
  selection calls neither callback — so the connect screen could sit on a
  spinner forever; the resolve is bounded at five seconds. Launch validation
  could hold the splash for well over a minute (four attempts against a 20 s
  connect timeout, twice over if rediscovery found a candidate) before Home,
  which has a Retry button, ever appeared; it is capped at twelve seconds.
  Changing quality mid-film rebuilt the player and silently reset the audio and
  subtitle selections to their defaults — those now live beside the A/V
  correction, which already survived. Safe insets are the union of the status
  bar and the display cutout, so a landscape hole-punch no longer sits under the
  back button. Tunneled playback is requested on television devices, where the
  SoC pipeline is what 4K HDR is built around, and not on handsets where some
  decoders refuse it. The tight focus bounds that surrendered Material's 48 dp
  minimum touch target are scoped to television, where there are no fingers.
  Search, sort, and filter survive rotation and process death. And the several
  `runCatching` blocks around suspend calls no longer swallow
  `CancellationException` — a screen that has left composition stops "recovering"
  from its own teardown.

- **Android release builds shrink from 18.5 MB to 3.4 MB.** `isMinifyEnabled`
  was `false` and `proguard-rules.pro` had been dead configuration since the day
  it was written, so every release APK shipped the whole of
  `material-icons-extended` — several thousand vector assets, of which this app
  draws about twenty — plus every library path the viewer never reaches. R8 and
  `shrinkResources` are on; the keep rules cover what is reached reflectively
  and nothing else: the generated kotlinx.serialization serializers for the wire
  models, and the generic signatures Retrofit reads off `PlurxApi`. Line numbers
  are kept so a crash report from a viewer's TV is still readable.

- **The decision says which subtitles can actually be a native HLS
  rendition.** `SubTrackDto.text` is `!is_bitmap_subtitle` — "there is text
  here to extract" — but the rendition path accepts a narrower set
  (`is_native_text_subtitle`: `subrip | srt | webvtt | vtt`), so `mov_text`
  and styled ASS/SSA arrived on the wire looking selectable and were not: an
  explicit pick answered 400 "the selected subtitle requires burn-in", and
  the native master had filtered them out to begin with. A 2160p WEB-DL with
  23 `mov_text` tracks offered 23 subtitles and could publish none of them.
  Every subtitle now also carries **`native`**, computed from the same
  predicate the master and the session create already gate on, so the wire
  answers both questions instead of one twice: `text` unlocks the extracted
  WebVTT sidecar (`/files/{id}/subs/{index}.vtt`, still served for `mov_text`
  and ASS/SSA — that is the route those tracks should take), and `native`
  unlocks a rendition. `text` is unchanged and still means what it always
  meant, so a client that reads only it behaves exactly as before.

### Changed

- **The library-first layout is now Catalog.** The picker, saved layout id,
  renderer registry, CSS scopes, UI regression golden, mockups, and design
  documents use `catalog`; browsers carrying the retired id migrate it on
  first load, so the rename does not reset an existing choice to Classic.

- **A subtitle track that cannot be extracted now fails fast and stays
  failed.** Extracting one is a full read of the source, so a wedged ffmpeg —
  a NAS mount that stops returning bytes without ever erroring — parked every
  waiting request forever, and a track that *failed* was remembered as
  nothing at all: AVPlayer asks for a subtitle segment about every six
  seconds, and each of those requests relaunched a multi-gigabyte scan of a
  file that had just failed. Extraction is now bounded at ten minutes and the
  ffmpeg process is actually killed at the bound — three times the worst
  honest cold extraction measured over a congested NAS, so nothing that would
  have succeeded is cut off. A failure is remembered for two minutes, which
  turns that storm into one attempt per window while staying short enough
  that remounting the share or replacing the file is picked up without a
  restart. And a sidecar larger than 8 MB is refused at publish instead of
  landing in the cache: real WebVTT is kilobytes, a dense SDH track for a
  three-hour film is about 200 KB, and anything at that size is a mislabelled
  stream this file gets re-read whole for on every segment request.

- **The hot control path stops re-doing a session's whole history.** Flow
  control runs on every published segment and every frontier advance; each
  run re-parsed the complete growing playlist, re-measured every segment it
  already knew, re-statted every pruned file for an ENOENT, and read the
  same three settings rows through the store's single serialized
  connection. The segment index is append-oriented now, pruned segments are
  never statted again, the ahead-window limits are a snapshot with a
  two-second staleness bound, and read-only settings and metadata lookups
  run on dedicated read connections instead of queuing behind the writer.
  Subtitle extraction joins in: the VTT endpoint used to run ffmpeg over
  the whole source — a full NAS read of a 60 GB remux — on every request,
  and now files each track by source fingerprint beside the transcode
  cache and serves the file.

### Fixed

- **The server refuses an explicit subtitle burn that would discard HDR.**
  Clients now receive a specific 422 response when they ask to burn subtitles
  into probed Dolby Vision, HDR10, or HLG video. Playback accounting and the
  encoder are left untouched, SDR burns continue to work, and there is no
  override that can silently trade HDR for subtitles.

- **Skip Intro and Skip Credits stay compact at every Apple player width.**
  The marker action previously accepted the full playback-control width on
  iOS, turning a short button into a bar across an iPad or other wide player.
  It now keeps its natural width at the trailing edge above the transport,
  while narrow layouts and accessibility text can still compress the label
  instead of overflowing the player.

- **Playback now leaves the full-screen Apple player at a title's natural
  end.** The AVPlayer end notification previously updated progress but only
  reached the view when autoplay was enabled, and no-next/autoplay-off titles
  left their cover open forever. Natural completion, manual Close/Menu, and
  failure-screen Close now share one idempotent exit sequence: restore iOS
  status-bar/Home-indicator preferences, clear player overlays and pending
  timers, detach Picture in Picture and subtitle layers, remove observers and
  remote commands, release the AVPlayer item/audio session/HLS session, then
  dismiss on the following main-loop turn. Online episodic autoplay still
  swaps to a discovered next episode; offline playback and a missing successor
  dismiss normally. An end notification with no catalog or item duration now
  reopens the growing stream instead of treating a temporary playlist end as
  proof that the title finished. Autoplay handoff keeps the audio session
  active, teardown cannot re-create Picture in Picture or resurrect a stale
  status poll, and the completion turn survives the coordinator being released.

- **The full-screen iOS player now retires system chrome after its own
  overlays leave.** Noirr draws video with a custom `AVPlayerLayer`, so hiding
  the SwiftUI transport never asked iOS to hide the separate status bar or
  Home indicator. In a full-screen iPhone or iPad window, both now remain while
  controls, playback info, a failure, a notice, or recovery/next progress is
  visible, then leave the video surface after the last player overlay fades.
  Paused playback keeps the existing four-second fade. A windowed iPad status
  bar remains system-owned; dismissing playback restores ordinary app chrome,
  and tvOS is unchanged. The physical-device checklist carries the remaining
  full-screen, Split View, Picture in Picture, and multitasking-control checks.

- **HLS ahead-window holds keep the measured Apple runway and now explain
  their active bound.** AVPlayer fetched about 120 seconds ahead despite a
  60-second preference, so retaining only 120 seconds behind that frontier
  could move the sliding playlist boundary onto the playhead. Live retention
  is now 180 seconds: the measured lead plus 30 seconds of back buffer and 30
  seconds for retry/reload. Time flow control still holds above 180 seconds and
  releases at 150 seconds; byte and global scratch holds still release at half
  their limits. A one-second time limit now releases at one second instead of
  degenerating to a disabled zero threshold. Session status and the web/Apple
  overlays identify `time`, per-session `bytes`, or `global` as the active hold
  reason and show its matching release value. A real-segment regression proves
  that a client frontier advance releases a held encoder, and a second test
  pins the unchanged-state guard so playlist polls cannot re-signal ffmpeg or
  reset the stall watchdog. This is not a structural escape when a client stops
  fetching: already-published media remains available, and the producer may
  remain held until the client advances. The observed iPad fetch-loop stop and
  the EVENT-to-sliding-playlist transition remain device-capture questions, not
  a server-threshold fix. The wider retention grows the default per-session
  media span from about 300 to 360 seconds, so the 8 GiB global scratch cap may
  bind roughly one 4K stream sooner.

- **An Apple client that remains stuck buffering now recovers visibly and
  leaves evidence.** After a title has rendered for five seconds, a sustained
  AVPlayer buffer wait gets one reconnect of the exact same delivery; an
  immediate repeat stops with connection-specific copy instead of mentioning
  a format switch that never happened. Cold-start waits remain outside the
  watchdog so a server filling its first HLS publish window cannot trigger a
  duplicate session. Each recovery reports its delivery method, film
  position, and measured stall duration through the bounded client log, while
  buffering remains ineligible for the HDR/codec fallback ladder.

- **A cache budget sweep can no longer delete a film while somebody is
  watching it.** Finished pre-transcodes now carry process-local read
  ownership from the cache lookup through the HLS session's lifetime. LRU,
  stale-claim, and orphan cleanup claim the same recipe exclusively before
  removing its row or bytes: an active reader is skipped until the next sweep,
  while a lookup arriving during deletion becomes an ordinary cache miss.
  Location deletion is scoped by storage class so a stale local copy cannot
  erase a future shared one. The
  `plurx_cache_protected_entries{reason="active_playback"}` gauge exposes the
  entries temporarily protected from housekeeping. Concurrent sweeps now end
  a losing budget pass instead of independently evicting the full deficit.
  This closes the documented reader/cleanup races without
  treating offline pins as evidence that online playback is safe.

- **Apple playback info now stays open when the transport controls fade.**
  iPhone, iPad, and Apple TV used one visibility state for both overlays, so
  the four-second control timer also dismissed the readout a viewer had opened
  to watch. The controls and readout now have independent lifetimes; the
  readout remains visible until it is explicitly closed, matching Android and
  the web player.

- **Coming Soon no longer leaves not-yet-local titles with blank cards.** The
  calendar already carried each item's provider poster path, but plurx threw
  that field away and only showed artwork when the same title could be
  resolved to an existing local library item. Future films and series sourced
  through TVmaze therefore stayed on initials, and refreshing library artwork
  could not help because those rows were not missing library art — some did
  not exist in plurx yet. Local artwork still wins; otherwise plurxd now
  downloads the TMDB, TVmaze, or Open Library poster into its own cache before
  returning the rail. Provider URLs remain server-side, downloads are
  de-duplicated and bounded, and the host/redirect allowlist keeps the calendar
  from becoming an SSRF surface.

- **A subtitle extraction could run twice, and the second run raced the
  first's own output.** The extraction registry deduplicates by cache key, but
  a request read the cache, missed, and only then reached the registry — and
  an extraction that finished in that gap left nothing to find: the owner
  renames its sidecar into place and *then* drops its registry entry, so the
  arriving request saw an absent flight and trusted its own stale miss. It
  became a second owner and re-ran ffmpeg over the whole source, against a
  file already on disk, publishing over the top of it. The window is small
  and the symptom is only a duplicated NAS read, which is why it survived:
  the repo's own dedup test caught it about one run in three and looked
  flaky. The decision is now made in one place under the registry lock, which
  is the only ordering point that has both facts — the rename happens-before
  the entry is removed, and the removal happens-before the lock is acquired,
  so a key with no flight registered is a key whose sidecar is either visible
  or genuinely absent. A failed extraction still leaves neither, and the next
  request still retries it.

- **Android: an MP4's subtitles were all offered and none of them worked.**
  A 2160p WEB-DL with 23 `mov_text` tracks listed every one of them, and every
  pick answered 400 "the selected subtitle requires burn-in". The client was
  deciding what could be an HLS rendition from `SubTrackDto.text`, which is
  `!is_bitmap_subtitle` — "has extractable text" — while the server decides
  from `is_native_text_subtitle`, which excludes `mov_text` and styled ASS/SSA
  because their authored positioning and typography do not survive WebVTT. The
  decision now carries the server's own answer as `native` alongside `text`,
  and the client routes on it: one predicate, consulted by the rendition
  routing, the menu's "Burn-in" label, the cold-start policy, and the ordinal
  that maps a stream index onto the master's rendition order — the last of
  which was the quiet one, since counting a `mov_text` track that the master
  never carried shifted every rendition after it and handed the viewer a
  neighbouring language. On a session those tracks now burn, as the server
  always intended; on direct play they stay free, read from the container.
  Compatible both ways: `native` is nullable, and a server that predates it
  leaves the client's codec table deciding exactly as before.
- **A 2160p 10-bit HEVC film played in the browser as a black screen with
  working sound, and nothing ever recovered.** Three faults in a row, each
  harmless alone. The player asked MediaSource about a *list* of HEVC codec
  strings and took any yes — but `hvc1.1.6.*` is Main (8-bit) and
  `hvc1.2.4.*` is Main10, so an 8-bit-only decoder's yes to the 8-bit
  strings routed a 10-bit source through MSE, where the decoder accepted the
  stream and rendered nothing. `bit_depth` had been on the wire the whole
  time and no decision read it; it does now, and a 10-bit source is asked
  only about Main10 (which also makes the `hevc10` ladder reachable — it was
  keyed on ffprobe's `codec_name`, which is always plain `hevc`, so it had
  never once been selected). Then nothing rescued it: on the hls.js
  transport a media error never reaches the `<video>` element at all, so a
  fatal decode failure ended at a toast with a dead player — it now restarts
  as a transcode at position, once per item, exactly as the progressive path
  has always done, and only for media/codec failures (a network fatal is not
  something a different encode fixes). And the guard on both rescues asked
  `PLAYER.started`, which the first `playing` event sets — **audio alone
  fires that**, so the rescue was disabled in precisely the
  black-picture-with-sound case it exists for. It now asks whether the
  decoder has actually produced frames. Nothing here changes what the server
  is told: the browser still claims everything it claimed before, still gets
  offered everything it was offered before, and the bit-depth check picks
  only which *transport* carries the same bytes.
- **Every Dolby Vision film played at the Auto rung in Chrome, and untouched
  in Safari.** A DV disc remux usually carries an HDR10-compatible base
  layer, so a browser reporting HDR support looked able to take it. Chrome
  refuses the track outright (`MEDIA_ERR_DECODE`) — what reaches its decoder
  is flagged Dolby Vision whatever the base layer looks like — so the
  player's error path rescued it into a transcode, and a 4K remux played at
  1080p with nothing anywhere saying why. Safari, which decodes DV, played
  the same files perfectly; that difference is what named the cause. Three
  fixes. The player now probes Dolby Vision separately from HDR (`dvh1`/
  `dvhe`) and says so, because HDR support was never the same question. The
  decision uses it: a DV source a browser cannot decode is a **strip remux**
  where ffmpeg can remove the DV configuration (video untouched — the viewer
  keeps the source's pixels) and a re-encode only where it cannot, decided up
  front instead of after a failed decode. And the strip capability now comes
  from the daemon's own record of which ffmpeg it runs, not from the
  *pre-transcode cache config* that happened to carry a copy of the version
  string — a server with no cache configured answered "no `dovi_rpu`"
  whatever ffmpeg it was running, which is how the strip came to be skipped
  on a build that supported it.
- **"Can this server strip Dolby Vision" is now observed rather than
  inferred.** It was decided by parsing the ffmpeg version line, which is a
  proxy nobody can check from outside the machine — and it now decides a
  playback *verdict*, not just a filter argument. The daemon asks its own
  ffmpeg at boot (`-bsfs`), logs the answer with what it costs when missing,
  and reports it in `/api/v1/system`. The container asserts the same
  capability at build time: the `jellyfin-ffmpeg7` install is deliberately
  unpinned so it tracks current, which means a stale image silently loses
  `dovi_rpu` — that is a failed build now instead of a 4K film quietly
  playing at the automatic rung.
- **A transcode that wedged mid-film was nobody's problem.** The stall
  watchdog declared victory at the first playable segment (and on two other
  early exits), so a pipeline that froze at minute 40 just drained the
  client's buffer into a hang on a segment nobody was writing. One watchdog
  now runs for the life of every encoder: suspension pauses judgement
  instead of ending it (and resume restarts the stall clock, so a long
  SIGSTOP can never be read back as a stall), EOF and kills end the watch
  without a verdict, and a mid-stream wedge fails the session so the
  player's recovery paths get their chance.
- **The global scratch cap did not measure the disk.** It summed each
  session's bytes *ahead* of the client while retention deliberately keeps
  another two minutes of media *behind* every frontier — so several healthy
  sessions could exceed the documented 8 GB ceiling by a whole retention
  window each. The cap now counts total live bytes; pacing keeps using the
  reserve, because they answer different questions.
- **Auto on a software encoder advertised 720p for smaller sources.** The
  filter never upscaled, but the bitrate target and the response metadata
  described 720p for a 480p stream. Both encoder arms of the Auto rung now
  follow the source down.
- **Two concurrent session creates with the same `request_id` could both pass
  the idempotency check and spawn two encoders.** The map recorded a create
  only after it finished — check, release the lock, spawn ffmpeg, record — so
  concurrent retries raced through the gap, and the loser could be handed a
  session its twin's supersession had already killed. A `request_id` is now
  *reserved* before any work starts; a concurrent identical create finds the
  reservation and waits for the first one's session instead of starting its
  own. A create that fails — or whose caller vanishes mid-await — clears its
  reservation on the way out, so a died attempt never poisons its id.
- **The hardware→software fallback kept its hardware slot for the session's
  whole life.** A session that fell back to a software encoder held the GPU
  slot it was admitted with until teardown — with the cap at one, the next
  hardware start queued behind a session no longer using the GPU, potentially
  for a whole film. The slot is now released at the transition, and the
  session's admission class flips to software at the same moment, so the
  speeds measured from the replacement encoder stop being filed as evidence
  about hardware (which was quietly poisoning the very record admission
  decides software-viability by).
- **A 4K film with forced subtitles played at 1080p whatever the quality menu
  said — while the overlay claimed "no video transcode".** Bitmap subtitles
  (PGS/VobSub) can only be shown by drawing them into the frames, so selecting
  one — including the player auto-applying a forced-flag track, which is every
  foreign-dialogue disc remux — restarts the stream as a transcode. That
  restart sent no target height, and the server's Auto rung answers
  `min(source, 1080)` on hardware: the right bandwidth call for Auto,
  silently overriding an explicit Original. Three fixes, one per lie. A
  transcode opened while Original is forced now carries the source's own
  height (the server still clamps to [144, 2160] and never upscales), so the
  burn re-encodes at source resolution. The same rule turned out to owe Auto
  the same honesty: an Auto burn over a remux/direct verdict also keeps the
  source's height, because the decision had already chosen to send this
  client the full-resolution stream — the burn adds an encode, not a
  downscale, and its 4K rung costs *less* bandwidth than the remux it
  replaces. Auto's `min(source, 1080)` cap still governs burns on genuine
  transcode verdicts, where it is the bandwidth call it was designed to be
  (PERF-PLAN §4.7). Turning subtitles off now goes back
  through the playback decision instead of opening yet another transcode: it
  restores the remux/direct-play (and the resolution) the burn took away,
  where it used to park the viewer in a burn-less re-encode forever — with a
  one-shot guard so the restart doesn't re-apply the default track and burn
  it straight back in. And the stats overlay's Reason row now describes the
  burn session itself rather than quoting the file's `/decision` verdict —
  it used to read "forced original quality (no video transcode)" over a
  running QuickSync re-encode, which is the line that sent the diagnosis in
  the wrong direction. The error-fallback owed the same honesty: a remux
  stream the browser refuses falls back to a transcode silently, and when a
  cached encode makes that swap instant, a film reads as "only plays 1080p"
  with nothing anywhere saying why — the fallback now writes the overlay's
  Switched row with the browser's own error, the way the decode-rescue
  already did. Caveat stated where it belongs (PLAYBACK.md): a burn
  composites in system memory, so a 2160p burn runs the CPU filter chain,
  and whether a given box holds realtime there is a measurement to take, not
  a promise this change makes.

### Changed

- **plurx cuts the copy path's segments now, not ffmpeg.** On an HEVC or
  H.264 source, a copy-video HLS session runs ffmpeg with no HLS muxer at
  all: it writes one continuous fragmented stream and the server decides
  where the segments end. It ends them only in front of a keyframe a player
  will not discard a frame at — the leading-picture discard that costs one
  frame at every ordinary boundary on an open-GOP disc remux. Where a
  stretch of film offers no such keyframe within 48 MB or 15 s the cut is
  taken anyway and counted, so the residual is stated rather than hidden;
  `scripts/gop-census <file>` measures how often that happens on your own
  library. Nothing about the delivered stream changes otherwise: the same
  `init.mp4`, the same segment names, the same playlist, and the decoded
  frames are bit-identical to the unsegmented stream (asserted by
  `framemd5`, both tracks). A source this reader cannot follow falls back to
  ffmpeg's own muxer automatically, once, before anything is published — so
  the worst case is the previous behaviour. Segments have the same box shape
  ffmpeg's own muxer wrote — `styp sidx sidx moof mdat`, one `trun` per
  track, and the same tracks (chapters are no longer muxed in as a text
  track, which Safari refused outright) — so the only thing a browser sees
  differently is where the boundaries fall. Measured on *Wicked* (2024), a
  4K remux at 58 Mb/s: 1.9 dropped frames a minute against 8.0 for ffmpeg's
  own muxer, a 4.2x improvement. `scripts/gop-census --sweep <file>` reports
  the same figure for any file in your library.
- **Copy-path segments grew from a 2 s floor to 6 s.** Every segment start
  costs one frame on an open-GOP source — the player treats it as a
  random-access point and discards the leading picture, which was the last
  remaining stutter on 4K disc remuxes in both Chrome and Safari. Keyframes
  *inside* a segment decode continuously and lose nothing (proved by playing
  the same stream unsegmented: zero drops in 1781 frames), so fewer
  boundaries means proportionally fewer drops — about 3× fewer on a typical
  1.75 s GOP. Start time is unaffected: the copy path bursts its first 90
  seconds at disk speed, so even a 6 s first segment exists almost
  immediately. Transcode segments stay at 2 s, where the encoder produces in
  real time and a closed GOP has nothing to lose at a boundary.

### Fixed

- **A 4K subtitle burn ran the CPU tone-map below realtime — buffering every
  two seconds on hardware whose GPU graph was probed at 4.9×.** Playing the
  burn at source resolution exposed the routing: *any* burned subtitle sent
  the session to the CPU chain ("composited in system memory"), and Dolby
  Vision did too, so a 4K DV film with forced PGS subs did its float
  tone-map on the CPU at ~0.9× while the viewer's buffer sat pinned at one
  segment, rate-chasing at 90%. Three routing truths replace that. A bitmap
  burn now keeps the proven GPU graph: scale + tone-map on the GPU pinned to
  the overlay's exact frame (two filters, one geometry), one download for
  the composite, and the encoder's upload after it — text burns still take
  the CPU chain, because libass lives there. A Dolby Vision source whose
  base layer is HDR10-compatible (Profiles 7/8 cross-compatible — the
  probe's label says so) now routes as the HDR10 stream its base layer is:
  it decodes to ordinary PQ with static metadata, and neither tone-map chain
  reads the RPUs, so declining the 4.9× graph for it bought no correctness;
  profiles without that base (5) still decline. And a latent ordering bug
  died on the way: the encoder's upload suffix used to be appended *before*
  the overlay in every burned graph — upload, then composite — an order only
  the suffix-less software encoder could survive, which is exactly what the
  tests used to burn with. Whether a given box holds realtime on a 2160p
  burn remains a measurement (the graph is new to the fleet); the chain it
  now runs is the one that measured 4.9× faster.
- **Playback stopped for a few seconds at the same spot in every film — the
  client was losing a race the server didn't know it was running.** With
  the publish gate, the first playlist a player loads is a cushion of
  media — 12 s or more — while hls.js buffers only ~9 s of it and parks.
  hls.js reloads a live playlist on demand rather than on the
  targetduration cadence, and it stretches the reload interval when
  playback sits far behind the live edge, which is exactly where the gate
  puts every session on purpose. So the reload that would have revealed
  the next segment raced the playhead to the cushion's edge and lost:
  `bufferStalledError` at the edge, a several-second freeze at the same
  position every play, then recovery when the backoff finally re-polled.
  Safari never stalled on the identical stream — AVFoundation polls every
  targetduration unconditionally. Diagnosed end to end by the new beacons
  (supply stall with 0.1 s runway; `bufferStalledError at 12.4s`), the
  session's own playlist (first-playlist edge at 12.471 s, the stall
  position exactly), and a headless harness that reproduced the stall on
  synthetic media from the playlist shape alone. The fix restores the
  cadence Safari always had: while the playlist is live and hls.js has
  not refreshed it for ~half a target duration, the player asks for a
  refresh through the loader's own entry point. In the harness that took
  the stall from 7.8 seconds to zero, with every segment fetched on the
  first poll after it existed.
- **A remembered decode limit could never be forgotten.** The player remembers
  a stream a device measured itself unable to hold smoothly, so Auto routes
  straight to a transcode instead of stuttering first. Clearing that memory
  required the measured decode figure to fall below 60% of the frame budget —
  but that figure measures how deep the decoder's pipeline is, not how hard it
  is working, and it gets *bigger* as playback gets healthier. So a device that
  had stopped needing the rescue could never say so, and Auto stayed pinned to
  a transcode for good. Clearing is now the trigger inverted: a minute of
  explicit-Original playback with almost no visible faults lifts it.

  And because this is invisible state that steers playback, automatic
  clearing is no longer the only way out: the player's **Quality** menu names
  the measurement when one is steering the stream and offers to forget it,
  **Settings → Measured playback limits** lists everything this browser
  carries and clears the lot, and entries expire on their own after 30 days.
  A verdict about a device, a browser build and a stream at one moment should
  not outlive all three.

  Every one of those exits still needed the viewer to know the mechanism
  exists, so Auto now re-tests by itself: an entry older than a week is
  ignored for one session, which plays the source and finds out. A clean
  minute clears it — 4K comes back with nobody having touched anything —
  while hitches re-stamp it and push the next re-test out another week. The
  Quality menu says so while it is happening, rather than claiming a
  transcode the viewer is not being given.
- **Opening a menu closed the playback-info overlay.** The two panels took
  turns, which is right on a phone (each wants most of the screen, and
  stacking them left neither dismissible) and wrong everywhere else: the
  reason to have the overlay open during a quality change is to watch what
  the change does to it, and it was gone before the menu appeared. On a mouse
  they now coexist — the overlay closes when you close it — and the menu
  slides clear of the panel instead of covering it, overlapping only in a
  window too narrow to hold both.
- **Chrome could freeze on a 4K remux, because the buffer budget forgot that
  an append is not free.** The client sizes its MSE buffer in bytes against the
  browser's ~150 MB quota, and it sized only what the browser would be
  *holding* — while the quota is charged on what it holds *plus the segment
  being appended*, both of which exist at once during an `appendBuffer`. That
  squeaked under while segments were 2 s and 15 MB. Once plurx started cutting
  its own segments at a 6-second floor, a segment at 61 Mb/s became 46–64 MB,
  every append asked for ~190 MB, and Chrome refused. hls.js answers a refusal
  by evicting, halving its own target and retrying — and gives up fatally after
  three failures on the same segment, which is a film that stops and will not
  resume without leaving the stream. The budget now reserves room for the
  segment in flight (`forward + one segment + back <= 144 MB`) and takes the
  segment length from the playlist rather than assuming it, so it follows
  whatever the segmenter actually cut. Safari was never affected: native HLS
  uses none of these numbers.

  Sized from the segment's real BYTES, not from its duration. The first cut of
  this budgeted `TARGETDURATION x bitrate`, which is wrong in the one direction
  that matters: copyseg cuts on 64 MB **or** 15 s, whichever comes first, so a
  15-second segment exists precisely because that stretch was quiet enough to
  fit 15 s under 64 MB. Costing it at the 69 Mb/s average said 129 MB — not
  merely wrong but impossible — and the client answered by running a 4-second
  forward buffer on a 4K remux, which stuttered worse than the bug this
  replaced. The server's byte ceiling is now a hard cap on the estimate, and
  the largest segment actually appended (from hls.js `FRAG_LOADED`) replaces
  the estimate entirely as soon as one arrives.

  Two smaller things fell out of the same investigation. hls.js takes the
  *larger* of the seconds target and `8 * maxBufferSize / bitrate`, so leaving
  its stock 60 MB byte target in place made it a floor under the buffer rather
  than a cap on it — it now moves with the seconds. And the back buffer is
  capped at 12 MB rather than 32 MB: a seek starts a fresh session on this
  path, so the back buffer buys a second or two of scrubbing and was otherwise
  a fifth of the budget spent on video nobody would watch again.
- **A film froze for several seconds, at the same second, every time you
  played it — playback now starts behind a cushion instead of at the live
  edge.** *Wicked* in Chrome stalled 8.8 s at 9.2 s in; Safari saw the same
  event as a 597 ms hiccup. The network was delivering at 137 Mb/s and the
  buffer was healthy either side of it, which is why it survived a day of
  looking at buffer numbers.

  A segment is invisible until it is cut, so the client's view of the stream
  advances in steps of one segment however smooth the producer is. Delivery
  to the browser runs at hundreds of Mb/s against a producer reading a 4K
  remux off the NAS at not much over real time, so a player that starts on
  segment zero drains everything published within seconds and is then pinned
  to the producer's publication rate — and the first publication it has to
  wait out whole *is* the freeze: exactly one segment's production time,
  once per film, always at the same second, because where the buffer first
  runs dry is a property of the film's opening bitrate. After that stall the
  producer's accumulated lead covers every later gap, which is why it never
  happened twice. Shrinking segments was tried first and only scaled the
  freeze — a 6 s ceiling moved it from 9.2 s in to 6.0 s and cut it from
  8.8 s to 5.5 s, proportionally, an asymptote rather than a fix — so that
  change is undone: the ceiling is back at 15 s and the floor at 6 s, and
  low-bitrate sources keep the boundaries (each one a dropped frame on an
  open-GOP disc) that the shrink was spending on nothing.

  The actual fix is a head start. A copy session now withholds `index.m3u8`
  until twelve seconds of media exist, so the first playlist a player loads
  is already a cushion deeper than the worst publication gap measured, and
  the client never catches the producer at all. The cushion lives on the
  server — the browser's quota-bounded buffer stays exactly as
  `bufferTargets` sizes it, it just always has published-but-unfetched
  segments in front of it. The server holds the playlist request open while
  the gate fills rather than 404ing (verified against the vendored hls.js:
  a manifest's first byte is waited on indefinitely, a 404 forgiven exactly
  once), and end of stream overrides the gate, so a film shorter than the
  cushion publishes whole, `ENDLIST` and all. The cost is honest:
  time-to-first-frame rises by the cushion's production time — spent once,
  at open, behind a spinner, instead of a freeze mid-scene at a moment the
  film chose. One more door opened on the way: the legacy-muxer fallback now
  stays available until the playlist is published rather than until the
  first segment lands, because segments no player has ever been told about
  are not a timeline anyone holds.

  The gate's first day found the two things that made that spinner longer
  than it had any right to be. It shipped as a segment *count* — three — and
  a count multiplies by whatever the opening cuts: a title quiet enough for
  the 15-second duration ceiling to bind turns three segments into a
  32-second cushion, so the gate is a duration now and promises twelve
  seconds of media whatever the title's segments happen to be. (The film
  that exposed the 21-second first frame, *Tron*, turned out to cut clean
  7.5–8.5 s segments — its cushion was ~17 s under either unit, and most of
  its 21 s was the pacing below plus a cold NFS open of a Dolby Vision MKV;
  a warm replay started visibly faster with nothing changed.) And producing
  any cushion is only fast if ffmpeg's `-readrate_initial_burst` exists to
  let the paced session start flat-out: the flag needs ffmpeg 6.1
  (jellyfin-ffmpeg7, the Docker image's engine, has it), and a build
  without it fills the gate at a flat 2× however fast the storage is — an
  encoder speed pinned at exactly 2.00× during startup is the tell, and the
  session's logged ffmpeg args show the flag present or absent outright.
  plurxd now warns at startup when pacing runs without the burst, because
  the gate turned that quiet degradation into seconds of every open.
- **A playback that stopped for seconds at the same spot every play
  reported nothing — twice over.** The same reference file, the same
  evening: buffer full at 10–15 s, ~500 Mb/s measured to the client, 0–1
  dropped frames, hardware decode confirmed in `chrome://media-internals`
  — and a multi-second stop at 12.5 s of media on every single play, which
  the stats overlay showed only obliquely, as "18 fps rendered · 75%
  realized speed" while the detector's window still contained the stop.
  Every per-frame counter forgave it: no drops, no supply stalls, `slow`
  hitches accumulate on healthy sessions too — so the hitch beacon never
  fired. And hls.js *names* this class of event (`bufferStalledError`,
  `bufferNudgeOnStall`, `bufferSeekOverHole` arrive through the ERROR
  event as non-fatal), but the player's handler dropped every non-fatal on
  the floor: the stop explained itself on every play into a function that
  ignored it. Two measurement-only fixes. Non-fatal stall-family events
  are now forwarded to the server log with the media position and runway,
  first three per kind per session. And a `rate_chase` beacon fires when
  the realized playback rate over the hitch detector's ~10-second window
  falls under 90% of what was asked while at least four seconds sit
  buffered — once a minute while it persists, carrying the realized rate,
  rendered fps, runway, and the decoder forecast. Supply stalls can never
  wear either label (they require a near-empty buffer; these require a
  healthy one), and the rate measurement now clears on pause and seek so
  it can never fire on a stale window. What Auto should *do* about a
  sustained chase, and what the 12.5 s stop actually is, are decisions
  that now have evidence arriving instead of screenshots.
- **The server could not say which build it was running — again.** `.git` sits
  outside the Docker build context, so the image can only name its commit if
  the deploy passes `PLURX_BUILD_REF`. That was previously "fixed" by having
  Compose forward the variable and documenting that deploys must set it, which
  put the work on a human remembering an environment variable every time and
  failed silently when they didn't. Nobody did: the System page read
  `0.2.0 (unstamped build)` for weeks of daily deploys, and three debugging
  sessions stalled on nobody being able to say what was running.

  `make docker-up` is the Compose deploy now — and it `cd`s into `deploy/`
  rather than passing `-f deploy/docker-compose.yml` from the repo root.
  Passing `-f` turns off Compose's automatic override discovery, so
  `docker-compose.override.yml` is silently ignored: the stack comes up with
  no media mounts, no GPU passthrough, and the transcoder fallen back to
  software. A convenience target that is not equivalent to the command it
  replaces is a trap, and this one sprang on the first real deploy. It is one
  command that stamps the commit itself, and every doc points at it instead of
  at a variable to remember. (It is not called `deploy`: bare metal and
  systemd are equally supported, and they
  stamp themselves from their own checkout because `.git` is right there.) A
  build that still ends up unstamped reports its **compile time** instead of
  the word "unknown", because "did my change land?" is answerable from a
  timestamp. And
  the build is now the first row of the player's stats overlay, where it can be
  read without an admin login and appears in any screenshot — the page and the
  binary are the same artifact, so one line answers for both.
- **The decode rescue was firing on a number that condemns every stream.** It
  switched an Auto session off the original when the measured decode figure
  reached 80% of the frame budget and a few hitches had been seen. That figure
  is `processingDuration` — queue latency on a pipelined hardware decoder,
  which grows as the pipeline gets *healthier*. The proof arrived as a
  screenshot: a 1080p transcode at 7.5 Mb/s with **zero** dropped frames
  reporting 83 ms/frame against a 42 ms budget, the identical "no slack"
  verdict as the 4K remux it had just moved the viewer away from. With that
  gate always true, the rescue was in practice firing on four hitch events in
  twenty seconds, which any two-hour film produces for a dozen transient
  reasons.

  It now judges frames the viewer actually lost — never presented or presented
  out of order — as a rate over a long window: 150 s of playback, 15 lost
  frames, 6 or more a minute. Clearing is that same measure inverted rather
  than a second heuristic, which fixes a related bug where a session the viewer
  called flawless could not clear its own entry (six compositor holds and three
  lost frames over two and a half minutes counted as nine "faults" against a
  bar of four). Entries written by the old gate are discarded rather than
  migrated: they were produced by a test that reads true on everything, so they
  are not measurements. And the overlay's Decoder row states the pipeline
  figure flat and grey instead of in warning amber — a number that accuses
  every stream should not look like a diagnosis.
- **Nothing said why a film had dropped to 1080p.** The rescue announced itself
  in a toast, which vanishes, and the Reason row is the server's verdict on the
  *file* and does not change when the client switches — so a viewer who looked
  five minutes later found no explanation anywhere. The stats overlay now
  carries a **Switched** row, with the numbers, for as long as that session
  lasts.
- **A buffer-quota problem could be remembered as a decode limit.** Quota
  pressure produces exactly what the decode rescue looks for — visible hitches
  on a copy session — so on Auto it fired, blamed the decoder, and wrote a
  per-device per-codec limit that then steered every future playback of
  anything that shape. That is how one browser taught itself that a fast
  machine could not play HEVC 2160p. A session the browser refused buffer on
  still falls back to a transcode, because a smaller stream genuinely fixes
  it, but no longer records a decode limit it did not measure.
- **The first copy segment could out-run the playlist.** `EXT-X-TARGETDURATION`
  is a live playlist's reload interval, and it was declared as the segment
  duration *ceiling* rather than what had actually been published — so a player
  loaded a playlist holding one nine-second segment, played it out, and waited
  the remaining six seconds for its next look at the playlist. Once per film,
  always at the same spot. The tag now tracks the longest segment published,
  and the first segment is deliberately short so the first reload lands with
  time to spare.
- **Changing servers on iPhone and Apple TV left the old server's bearer token
  on the device.** Only the in-memory copy was cleared, so an app killed
  between connecting to server B and signing in to it relaunched holding
  server A's token beside server B's address — and sent it there, in cleartext
  on a LAN, where B answered 401 and A's still-valid session was destroyed for
  answering a question it was never asked. The origin and the token are now
  written together or not at all, through one `SettingsStore` entry point that
  makes every caller name the token belonging with the address it is storing.
  A server rediscovered at a new address keeps its own token, because that is
  a move and not a change of identity.
- **Choosing a discovered plurx server on Apple could spin forever.**
  `NetService` delivers every outcome — the address, the failure, and its own
  five-second timeout — through run-loop sources, and the resolver was called
  from a cooperative-pool thread whose run loop is never spun, so no callback
  could ever arrive: the row stayed "resolving", every other row stayed
  disabled, and saved-session recovery sat on the bootstrap spinner. It now
  resolves on the main run loop, with a deadline one second past the service's
  own so that no future scheduling change can strand a caller again.
- **A 4K HDR disc remux no longer starts a transcode just because it has
  subtitles.** Cold start picked the first subtitle track matching the
  viewer's language whether or not anything had flagged it, so an
  English-audio film with an unflagged English track showed full subtitles on
  every play — and when the only English track was PGS, as on almost every
  disc remux, showing it meant burning it: an encoder slot per play, H.264,
  and the HDR gone. Automatic selection now applies a forced track (whatever
  its format — a film whose foreign dialogue is unsubtitled is not watchable),
  or a default-flagged text track through the free rendition path, and
  otherwise selects nothing. Picking a PGS track by hand still burns it, at
  source height, exactly as before.
- **A seek made during an Apple stream change no longer vanishes.** Two
  server-session replacements must not overlap — they share a `playback_id`,
  so the newer one deletes the older — but the guard enforcing that silently
  dropped any seek or track change that arrived while a replacement was in
  flight, which is what made the tvOS progress bar snap back after a burst of
  30-second step-seeks during a quality change. The newest request is now
  remembered instead, and replayed as exactly one trailing reopen when the
  change lands.
- **A quality or track change that fails no longer takes the film with it.**
  The old session was deleted before its replacement was requested, so a
  create that failed left the player pointed at a playlist the client had
  already removed: the buffer played out, then the stream stalled, and the
  error stayed quiet because an item still existed. The predecessor is now
  retired only once its successor exists; a failed change leaves the current
  stream playing, telemetry running, and surfaces a transient error instead.

### Added

- Playback stats: native HLS (Safari) sessions now attribute hitches to
  playlist segment boundaries, the "fps rendered" figure counts frames that
  actually reached the screen rather than Safari's decode-ahead, and the
  overlay reports the realized playback rate whenever it deviates from what
  was requested.

### Fixed

- **Android told the server nothing about Dolby Vision, so no Android device
  ever kept it.** Absent `dv` means no — the server says so in as many words —
  so every DV title on every Android box took the strip path, and a Profile 5
  source, which has no compatible base layer to fall back to, was re-encoded
  in full. The client now probes `MediaCodecList` for `video/dolby-vision`,
  maps the decoder's profile constants to Dolby's numbering, and claims the
  intersection of that and a display that actually shows DV — never dual-layer
  Profile 7, which stays on the server's strip path where it belongs. Nothing
  on the server changed; it was always deciding correctly from what it was
  told.
- **Lossless Atmos came back as 256 kb/s AAC on the boxes most likely to have
  a receiver.** Audio codecs were claimed only where the device had a
  *decoder*, and a Shield feeding an AVR has no TrueHD decoder — the receiver
  does that job. The claim is now the union of decoder support and what the
  *active route* accepts as a bitstream (Media3's `AudioCapabilities`, which
  reads the HDMI plug extras and, on API 33+, the direct audio profiles), and
  it is recomputed on every decision, because unplugging HDMI or switching to
  the TV's speakers changes the truthful answer. The whole probe also moved
  off the main thread, where it had been running binder IPC per play.
- **An explicit quality choice did nothing, and "Original" made things
  smaller.** The quality menu sent its rung as `force=720`, which the server
  parses as Auto, so picking 720p over a slow link silently direct-played 4K
  anyway; and an Original-quality subtitle burn sent no height at all, so a
  4K remux restarted as 1080p — the exact opposite of what the menu promised.
  `force` now says `auto`, `original`, or `transcode`, and the height rides on
  the session create, where a burn or Original carries the source's own height
  and only a genuine Auto omits it.
- **A subtitle cost the film its resolution and its HDR.** Every
  server-listed subtitle — including an SRT the server would hand over free —
  started a burn transcode, so choosing English subtitles on a 4K HDR remux
  swapped it for a tone-mapped H.264 re-encode. SRT and WebVTT now arrive as
  native HLS text renditions on a copy session that leaves the video stream
  untouched, switching between two of them changes only which rendition is
  selected rather than creating a server session, and only tracks with no text
  to send are burned in. The cold-start rule is now one function with the
  table in front of it: a forced track auto-shows whatever its codec, a
  default-flagged text track auto-shows for free, a default-flagged bitmap
  track does not, and a merely-same-language track never did belong there.
- **A cached transcode resumed at 0:00 and rebuilt itself on every scrub.**
  The session response's `vod` flag — the whole stream already on disk — was
  never decoded, so a cache hit started from the beginning and each seek threw
  away a complete encode to make another. A VOD session now seeks in place,
  like direct play; a live one still reopens, because a live playlist cannot
  be range-sought.
- **A stream the device refused was a black screen.** There was no
  `onPlayerError` anywhere, so a rejected direct or remux stream simply never
  started and said nothing. It now gets exactly one automatic rescue —
  reopened as a guaranteed-compatible transcode at the current position — and
  a second failure is a real error with Retry and Back. Decoder fallback is
  enabled too, so a flaky hardware decoder degrades to software rather than
  spending that rescue; the player also takes audio focus properly and pauses
  when the headphones come out.
- **Toggling "Autoplay next episode" mid-film restarted the film.** The
  listener effect was keyed on the preference, so changing it disposed the
  effect, released the live player, and re-registered on the released one —
  which restarted the episode at the position it had opened with. The effect
  is keyed on the player alone now and reads the preference as it stands.
- **Changing servers left the old server's token on disk.** Only the
  in-memory copy was cleared, so killing the app between connecting to a new
  server and logging in sent the previous server's bearer to the new one over
  plain HTTP — and destroyed the still-valid session on the old one when the
  new one answered 401. Origin and token are written together or not at all
  now, with one owner for the invariant.
- **The quality menu offered rungs that did not exist.** It was a hardcoded
  enum listing 2160p and 1440p for every file, including a 1080p source that
  can only be upscaled into them. It is built from the ladder the server
  advertises for that source, labelled with each rung's bitrate, falling back
  to the old list only for a server too old to send one. Separately, a merged
  collection's "Recently added" sort was a no-op: the client never decoded the
  `added_at` the server sends, so several shares arrived as sorted runs laid
  end to end instead of one interleaved grid.

### Added

- **Whether subtitles cost a session or a restart is the viewer's call now.**
  Any file on iOS/tvOS carrying an SRT/SubRip/WebVTT track opened through a
  copy-HLS session even when its video could have used the raw direct URL —
  identical picture, since the video is repackaged untouched, but a server
  session and a segmenter on every play where the web and Android clients pull
  raw bytes. It bought something real: every text track already exists as a
  rendition, so turning subtitles on mid-scene, changing language, and turning
  them off again never interrupt the picture. Settings → Subtitles → **Subtitle
  switching** now decides which of those two costs to pay. **Instant** is the
  default and is exactly what shipped, so an install nobody touches behaves
  identically; **After a short pause** direct-plays the file and rebuilds it as
  a copy session at the same film position the first time a subtitle is chosen —
  the same clean reopen a bitmap burn already takes. The choice is read once,
  when a title starts, so changing it mid-film never rebuilds the stream under
  the person watching; a track chosen automatically at cold start counts as in
  use, so it is visible from the first frame; and once a text track has been
  asked for the stream keeps its renditions for the rest of the title, because
  dropping back to direct play would be a second restart nobody asked for. A
  file with no native text track direct-plays under both settings — there is
  nothing a session could publish.

### Changed

- **The Apple client asks the server which subtitles can be renditions instead
  of guessing from the codec name.** `SubTrackDto` carries two booleans that
  are easy to confuse: `text` is "there are characters in here"
  (`!is_bitmap_subtitle`), while `native` is "this can be an HLS WebVTT
  rendition" (`is_native_text_subtitle`). They disagree on MP4 `mov_text` and
  on styled ASS/SSA, and it is `native` the segmenter enforces — a non-native
  track is absent from the HLS master and an explicit pick of one is answered
  with 400. iOS/tvOS re-derived that answer locally from a hardcoded codec
  list. It now decodes `native` and prefers it, falling back to the codec list
  only against a server too old to send the field, through the one property
  every caller already reads: the master rendition ordinal, the burn test, the
  cold-start policy, the direct-play guard, and the menu's "Burn-in" label. The
  two lists agree today, so nothing about playback changes — the point is that
  they can no longer drift, and that a track the server declines to publish can
  never shift the ordinal a viewer's pick resolves to.

## [0.2.0] — 2026-07-30

Historical development milestone; no `v0.2.0` tag was cut. Published releases
begin with 0.2.7.

### Added

- **Big remuxes stream in segments.** A remux at or above 40 Mb/s — or one
  whose storage can't stay comfortably ahead of it — is delivered as
  copy-video HLS instead of one progressive file, so Chrome's shallow
  progressive read-ahead stops being the ceiling on 4K playback. The video
  stream is untouched; audio is re-encoded only when the browser can't take
  the source. The player verifies the browser will actually accept the codec
  through MediaSource before taking the segmented path, and **Quality →
  "Original · one stream"** forces the old single-stream delivery when you
  want it.
- **The pre-transcode pass reports itself.** The cache producer — the only
  background job that holds an encoder for hours — now appears in the
  activity header and on the Activity page while it runs: which title, why
  it was chosen, how far through the pass it is, with an admin Stop that
  finishes the current title cleanly. Previously the only way to learn what
  was pinning the GPU was `ps` on the box.
- **Frame-level playback diagnostics.** The stats overlay now watches every
  presented frame and counts five distinct faults — backwards steps, held
  frames, late frames (a compositor hold), single dropped frames, and skips
  — with the spacing between them, what the player was doing within 150 ms
  of each one, the decode cost at the fault against the session's own
  typical, and the realized playback rate versus what was requested. The
  panel also names the transport outright and discloses when a browser
  gives the detector no frame callbacks at all.
- **Decode-margin rescue.** An Auto session on a copy path that measures
  itself out of decode slack — median decode at 80% of the frame budget or
  worse, with visible hitches, over at least 20 s — switches itself to a
  transcode at position and remembers the verdict per codec and resolution,
  so the next Auto play routes straight there and the Reason row says why.
  An explicit Quality → Original always overrides, and a clean explicit
  Original session clears the memory again.
- **Per-mount storage probe.** The server measures what each library mount
  can actually deliver — throughput, cold seek, a sustained trace replayed
  against a simulated client buffer — and reports it in the System page and
  the perf report, so "is the disk fast enough for this file" is a number
  rather than a guess.

### Fixed

- **One frame died at every segment boundary of a 4K HEVC remux.** The
  investigation is written up in `docs/STUTTER-4K.md`; the shipped fixes,
  in order: the copied stream no longer carries in-band parameter sets that
  its `hvc1` tag promises are absent (a spec violation handed to the
  decoder once per segment); a Dolby Vision source sheds its enhancement
  layer, RPUs, **and the container's DV Profile 7 declaration** — the
  declaration alone made Safari on Apple silicon refuse hardware decode of
  a stream it decodes natively (needs ffmpeg ≥ 7.1 server-side; older
  builds strip the data but keep the box); and the copy path no longer
  advertises `#EXT-X-INDEPENDENT-SEGMENTS`, a claim that is false for
  open-GOP sources and an invitation to discard one leading picture per
  segment.
- **The buffer asked Chrome for five times more memory than it grants.**
  Copied 4K streams targeted 60 s of forward buffer — ~775 MB of a 69 Mb/s
  film against a ~150 MB browser quota — so playback spent its life
  appending, being refused, and evicting near the playhead. Buffer targets
  for copied streams now derive from a byte budget; transcodes keep the old
  targets, which were always within it.
- **The server couldn't say which commit it was running.** `docker compose
  up -d --build` never passed the build stamp through, and the UI hid the
  resulting "unknown" as noise, so the System page read a bare version
  through weeks of deploys. Compose now forwards `PLURX_BUILD_REF`, an
  unstamped build says so on the page, and the deploy playbook stamps it
  automatically.
- Mistyping the password when creating an account no longer locks the new
  user out immediately: account creation and password reset ask for the
  password twice and require a match.
- Library pagination gained first/last buttons; the Libraries page warns
  when no metadata keys are configured and offers the artwork refresh
  directly; the stats overlay and storage block are legible again; home
  rails scroll with arrows and a hover scrollbar, like the rest of the
  noirr apps.

### Added

- **Home video & photos.** A fourth library kind, `home`, turns a folder tree of
  camera files — phone clips, camcorder dumps, scanned photos — into a library
  you can browse, play, and curate. The directory tree *is* the organization:
  `2019/Beach Trip/clip.mp4` becomes **2019** ▸ **Beach Trip** ▸ the clip, and
  titles are the filename verbatim, so "Christmas 2019.mp4" stays *Christmas
  2019* rather than being tidied into *Christmas* by the movie parser. Photos
  are first-class items with a full-screen lightbox (←/→, Esc, swipe), and are
  kept out of *Recently added* so a 2,000-photo import can't bury the home
  screen. Clips play through the ordinary pipeline — direct play, resume,
  quality menu, continue-watching — because a home video is just a video.
- **Capture dates.** Home items carry `recorded_at`, filled from the first
  source that has one: a Kodi `.nfo`, then the container's `creation_time`
  (EXIF `DateTimeOriginal` for photos), then a date on the filename, then the
  file's mtime. "Date recorded" is the default sort for home libraries.
- **`.nfo` sidecars, read exactly once.** A `<basename>.nfo` seeds an item the
  first time it is scanned and is then dead to plurx: never re-read, never
  written. Add a sidecar later and it still seeds on the next rescan; edit one
  after seeding and nothing happens, because the database owns the metadata by
  then and your own edits can never be clobbered by a file on disk. A junk
  sidecar complains once, by name, instead of on every scan.
- **Metadata editing** for home libraries (admin): title, date recorded,
  description, and tags, from a pencil on the item page. Tags are searchable.
  Movies and shows are still owned by their metadata agent and refuse the edit,
  with a message saying why — a hand edit there would vanish on the next
  refresh.
- **Local artwork for home libraries.** No provider is ever called for a camera
  file; art you put beside the media wins (`<stem>-thumb.jpg`, `poster.jpg` in
  a folder), otherwise ffmpeg grabs a frame 20% in, and folders inherit their
  first child's poster. Generated art lives in the artwork cache — plurx still
  never writes anything into your media folders.

### Changed

- Every drill-in page — item, library, category, search — now shares one header.
  The back control sits on the same line as the breadcrumb trail instead of
  floating above it on its own, and always points at the crumb one level up, so
  the button and the trail can no longer disagree about where "up" is.
- The breadcrumb trail records the list you arrived from. Opening a movie from
  the full Movies page now reads **Home / Movies / The Menu** rather than
  **Home / The Menu**, and the middle crumb returns you to the list — including
  for categories and search results. Show → season → episode hops keep it, so
  the trail stays complete all the way down. A deep link with no such history
  still degrades to **Home / Title**.
- 2160p content is labelled `2160p` rather than `4K`, matching every other
  resolution chip in the app.
- Durations under a minute now read in seconds. A six-second phone clip
  labelled "0m" was worse than saying nothing.
- Schema v6 rebuilds the `libraries` and `items` tables to drop their
  kind CHECK constraints — SQLite cannot alter one, and every future media type
  would otherwise repeat the same dance. Kind validation already lives in one
  place per enum. The migration runner now disables foreign keys around each
  migration and runs `PRAGMA foreign_key_check` afterwards, failing loudly
  rather than quietly leaving dangling rows. Existing databases upgrade in
  place; the upgrade is covered by a fixture test that builds a v5 database and
  asserts every row survives.

### Added

- **Scheduled jobs.** plurx can now do its own housekeeping: each library has a
  **scan** interval and a **refresh art** interval (Settings → Libraries, under
  the library's status), and the server has two maintenance jobs — *retry
  unreadable files* and *clean transcode cache*. Everything is **off by
  default**: upgrading a server must not silently give it new habits at 3am.
  Intervals are minutes with a floor of 15, since the scheduler ticks once a
  minute and a real library scanned continuously is a NAS denial-of-service on a
  timer. Per library rather than server-wide because a download folder and a
  finished archive deserve different cadences. When a scan and a refresh come
  due together the refresh wins — it already does everything the scan does, and
  running both walks the library twice for one due moment. Runs are stamped when
  they **finish**, on the library row: stamping the start would make a 40-minute
  scan on an hourly schedule due again 20 minutes later, and keeping the stamp in
  memory would mean a server that reboots nightly either scans every boot or
  never scans at all. Scheduled runs go through the same trigger as the buttons,
  so one can't stack on a scan already in progress; it simply waits for the next
  tick. The decision of what is due is a pure function with its own tests
  (`crates/plurxd/src/schedule.rs`), including the clock jumping backwards —
  which would otherwise park a schedule for as long as the jump.
- **Scan at startup** (off by default, Settings → Libraries). An interval
  schedule can only measure from when the process is running, so a server that
  was switched off overnight ignores everything that landed until its next
  scheduled run — or, with no interval set, until someone presses a button. This
  scans every library once, about 30 seconds after boot: late enough not to race
  the first plays of the morning for the same disks, and enough of a pause that a
  crash-loop can't become a scan-loop against the media volume.
- **Transcode cache cleanup.** The reaper has always cleaned up after sessions
  it knows about; it can't clean up after a SIGKILL, an OOM or a host reboot,
  which leave working directories on disk that nothing will ever claim. On a 4K
  library those are gigabytes, and the only symptom is a disk filling for no
  visible reason. The new job sweeps any directory under the transcode root that
  no live session owns.
- **A file whose probe failed can be read again.** A movie added while its
  permissions were wrong showed `Video —  Audio —` forever: the scan is
  incremental on size and mtime, and `chmod` moves neither, so every later scan
  skipped the file and nothing in the UI offered a way to ask again. Three
  things now close that: the scan retries any file with no media details (in
  place, against the item it already belongs to — re-deriving the item from the
  filename would orphan a home video renamed by an NFO or by hand); a
  **⟳ Reanalyze** button appears on the item page for admins, next to a plain
  explanation of why the dashes are there; and `POST /api/v1/items/:id/reanalyze`
  does the same over the API, reporting per file whether it was repaired, still
  failing, or gone. A scan that fixes something says so — *repaired* is its own
  count, inside `unchanged`, because nothing about the file changed on disk.
- **ffprobe now says why it refused.** It ran under `-v quiet`, so a failure
  produced an exit code and nothing else, and "Permission denied" was
  indistinguishable from "Invalid data found when processing input" — a
  permissions problem and a corrupt file, which have opposite fixes. Failures
  now carry ffprobe's own last line of stderr into the scan report and the log.

### Fixed

- **An item imported by another application never got artwork.** monarr POSTs
  `/api/v1/scan` the moment an import finishes; the handler placed the row and
  stopped. Enrichment lived only in the *full* scan, so a peer-ingested episode
  got a database row and a blank card, and stayed that way — the full scan that
  would eventually have fixed it is off by default. Both scans now go through
  one enrichment path, so they cannot drift apart again; the targeted one
  enriches exactly the items it placed (and the seasons, shows and folders their
  artwork is fetched through), because the caller is holding the connection open
  and waiting.
- **One transient TMDB failure marked an item permanently done.** The enrichment
  queue keys on `metadata_at`, not on `poster_path`, and `metadata_at` was
  stamped whenever the provider answered — including when the poster download
  inside that answer failed. A single 429 therefore meant a null poster forever,
  indistinguishable in the schema from "TMDB has no image for this". Attempts are
  now recorded with their reason (`artwork_attempted_at`, `artwork_error`), a
  half-hourly sweep re-fetches anything matched but pictureless, and each item
  waits a day between attempts so a permanently art-less film costs one request,
  not forty-eight. The sweep is **on by default** — a default of zero would ship
  the bug it exists to fix — and drains the backlog an upgrade inherits with
  nobody pressing anything.
- **Artwork self-healing stays responsive under a large or unhealthy backlog.**
  Retry work now runs outside the scheduler loop, admits one bounded batch at a
  time, and gives every never-attempted row a turn before reclaiming older
  failures. A missing API key, a provider outage, or a store read error no
  longer fabricates a "no image" result and hides the item for 24 hours; only a
  provider path that actually reached an artwork outcome records the daily
  backoff. TV retries preserve healthy ancestor metadata, remember a show id
  resolved for routing, cover hero backdrops as well as posters, and refuse to
  route an unnumbered episode through TMDB Specials.
- **TMDB rate limits no longer eat the rest of a scan.** A 429 or a 5xx was
  treated exactly like a 404: one attempt, permanent failure, artwork silently
  lost for every item after it, run reported as a success. Both are now retried
  with backoff, honouring `Retry-After`. A 404 is still a fast permanent no —
  TMDB has answered, and asking three times is three times the load for the same
  word.
- The crammed `102` form is understood. DVD-era rips write season and episode
  as three digits — `drawn.together.102-med.avi` is S01E02 — and plurx skipped
  every one of them. It is tried only after `S01E02` and `1x02` fail, on a
  standalone three-digit token, seasons 1–9 only: four digits are a year or a
  resolution far more often than they are season 10, and inventing an episode is
  worse than skipping one. Resolution and codec numbers are excluded by name
  (`264` would otherwise be season 2, episode 64), and `x264`, `1080p` and
  `480p` can't match at all because their digits touch a letter. No episode
  title is taken from this form — what follows the digits is the release group,
  and *Episode 2* beats *med*. Anime is deliberately exempt: there, three digits
  after a dash are the absolute episode number, so `One Piece - 102` stays
  episode 102 rather than becoming season 1, episode 2.
- Samples and other extras say that's what they are, instead of claiming the
  episode couldn't be identified. A skip note prints the whole path, so
  "no season/episode marker … in the file name or on the folder holding it"
  under `Drawn.Together.S01E02.DVDRip-MEDiEVAL/sample.drawn.together.102-med.avi`
  was the same self-contradiction as before, one layer down: the file was
  excluded on purpose and the message described a different rule. Skip reasons
  are now produced where the decision is made rather than reconstructed after
  the fact, so they cannot drift again. Extras are recognized two ways: the
  Plex/Jellyfin `-trailer` / `-featurette` / `-sample` suffix convention, and
  the words *sample*, *trailer*, *proof*, *screens* where an episode title
  cannot be — before the marker, or anywhere in a hash-named file that borrowed
  its folder's marker. *sample* counts anywhere, since no real episode title
  contains it; the others are left alone inside a title, so
  `Show.S01E02.The.Trailer.Park` is still an episode. This also catches extras
  that carry a perfectly good marker of their own (`Show.S01E02.sample.avi`),
  which previously attached to the episode as a second version and, tying on
  resolution, could be picked ahead of the real file.
- Episodes whose filename is a hash are found by their release folder. A common
  torrent shape puts every identifying token on the directory —
  `Drawn.Together.2004.S01E06.Dirty.Pranking.Number.2.480p.DVD.x265.Panda/` —
  and names the file inside `956a4a82d3e71a92e95bc3658e6978d7.mkv`. The parser
  only ever regexed the *filename*, so every one of these was skipped, and the
  skip note printed the full path while claiming there was no `S01E06` in it —
  a message that contradicted itself on screen. The marker is now read from the
  immediate parent directory when the filename hasn't got one, which is where
  the show title has always been read from anyway; the folder above the release
  still wins the show name, so a file under `Drawn Together/Season 1/` is still
  *Drawn Together* and not *Drawn.Together.2004*. Two things deliberately do not
  inherit a folder's marker: a `Season 02` directory, which carries a season but
  no episode, and a file whose own name says it isn't the episode (`sample`,
  `trailer`, `proof`, `screens`, `rarbg`) — a 30-second sample beside the real
  file would otherwise attach to the same episode and, on a resolution tie, sort
  ahead of it. The skip message now names both places that were checked.
- A scan that reports errors now says which files and why. The scan report has
  always carried two things — a count and a list of human-readable problems —
  but only the two library-wide failures (a missing root, an unreadable
  directory) ever filled the list. The two *per-file* failures did not, so a
  scan that hit a bad file showed a red **2 errors** in Settings with nothing
  underneath it and no way to find out which two of three hundred files were
  meant. Worse, one of the two logged nothing either, so the server log couldn't
  answer the question the UI had raised. Both now name the path and the cause:
  a file the walk listed but couldn't stat says whether it's permissions, a
  broken symlink or a file moved out from under the scan, and notes that the
  existing record was kept; a file ffprobe refuses says it was added without
  codec or duration, which is what makes its later playback decisions guesses.
  The rule is now an invariant on the report — no error is counted without a
  line describing it *and* a log line at ERROR level — and it is what the new
  tests check. ERROR rather than WARN so that setting the log view to **Errors**
  narrows it to exactly the files a scan complained about, which also means they
  are still findable after a few hundred lines of metadata fetching have rolled
  through the log's ring buffer.
- The scan counts no longer read as more events than there were files. A file
  ffprobe refused was counted as both *added* and an *error*, so two bad new
  files produced "2 added … 2 errors" — four numbers for two files, with nothing
  saying they were the same two. The counts are now explicitly two different
  kinds of thing. What happened to a file's record — added, updated, unchanged,
  skipped, or the new *unreadable* — is a partition: every file the walk found
  lands in exactly one, and they sum to the total, which a test now asserts.
  Whether it came through cleanly is a flag on top: the new *degraded* count is
  a subset of added and updated, and is displayed attached to them — "2 added
  (2 incomplete)" — rather than as an unrelated number further down the line.
  Moving those files out of *added* was the other option and would have been
  worse: the scan would report nothing added while two new items sat in the
  library, so the overlap is kept and simply stated.
- Skipped files are grouped by show instead of listed one per file. A show
  whose files carry no `S01E02` skips *every episode*, so one mis-named series
  produced two dozen near-identical lines and a real library produced 187 —
  which the ten-line cap then truncated into "…and 177 more", the least useful
  possible summary of a problem with exactly three causes. The report now says
  `skipped 168 files in /8tb/tv/Drawn Together — no season/episode marker`, one
  row per folder, worst first, expandable to a few of the filenames. Grouping is
  by the folder directly under the library root, because that is the unit the
  operator acts on: grouping any deeper reproduces the per-file list one level
  up. Counts still cover every file; only the samples are capped.
- Scan problems are boxed and scroll in place instead of printing into the
  libraries table. A TV scan reporting 187 skips pushed every other library off
  the screen and turned the settings page into a log dump; the list is now a
  fixed-height panel that scrolls, so a row is the same size whether its scan
  found one problem or a thousand. Long release-group paths wrap inside the box
  rather than stretching the column, and a panel holding real failures gets a
  red left spine that a skips-only panel doesn't — bad scans stay spottable at a
  glance now that the text itself is contained.
- Skipped files say which ones. "1 skipped" meant a file the library kind
  couldn't identify, most often an episode with no `S01E02` in the name sitting
  in a Shows library, and the only way to find it was to guess. Skips are listed
  by name, with what was expected, in quieter ink than the errors they sit
  beside — they are usually a stray trailer, not a problem. Both lists are
  capped so a pathological library can't produce a page-long status line, with
  a trailing count of what was left out; errors claim the cap before skips do.
- Closing the player while it is fullscreen now hands the screen back. Fullscreen
  belongs to the browser rather than to the page, so hiding the player's own UI
  did not end it: `#player` stayed the fullscreen element and the tab kept the
  whole display with nothing drawn on it. On a desktop that reads as a black
  screen you press Esc on out of reflex; an iPad has no Esc, only Safari's own ✕
  in the top-left corner, so until you find it the browser is unreachable and
  the app looks hung. Every exit runs through this path, including the automatic
  one when a film ends with nothing queued behind it. A picture-in-picture window
  is closed on the same occasion and for the same reason — it is the browser's
  window, and emptying the video element leaves it open, playing nothing. The
  fullscreen button can also now turn *off* iPhone-style video fullscreen, which
  it was able to enter but not see: `document.fullscreenElement` never reports
  that mode, so pressing the button a second time did nothing.
- The Docker deployment no longer shares a Compose project with anything else
  on the host that happens to live in a directory called `deploy`. Compose
  names a project after the directory holding its file, so plurx's was
  `deploy` — and two stacks that resolve to the same project name *are* one
  project: `docker compose ps` in either lists the other's containers, and `up
  --remove-orphans` or `down` in either **deletes** the other's, on the
  reasoning that they aren't in the compose file it just read. The container
  disappears rather than exiting, so there is no exit code, no restart, and no
  log left to read; `restart: unless-stopped` cannot help something that no
  longer exists. `container_name: plurxd` hid the collision rather than causing
  it, by keeping the service off the `<project>-<service>-1` naming that would
  have made it obvious. The project is now pinned to `plurx`, and the data
  volume to `plurx-data` rather than `<project>_plurx-data`. Read **Renaming
  the data volume** in `deploy/README.md` before upgrading: pointing plurxd at
  a volume that doesn't exist yet is not an error — Docker creates an empty one
  and plurxd initialises a fresh database in it, which looks exactly like
  losing your library.
- A `docker stop` no longer becomes a SIGKILL. plurxd drained in-flight
  requests without a deadline, and a playback response is a stream: a paced
  remux holds its connection open for a quarter of the film's runtime, and an
  HLS playlist poll is only ever moments from the next one. Waiting for all of
  them to finish meant never finishing, so Docker spent its ten-second grace
  period achieving nothing and then killed the process — which surfaces as exit
  137, indistinguishable at a glance from an out-of-memory kill. The drain is
  now capped at five seconds, comfortably inside that grace period, and logs
  what was still open when it gave up.
- Items are no longer marked watched minutes in. The web player treated the
  `ended` event as "the film finished", but `ended` fires at the end of the
  media the browser *has*, which is not the same thing: a remux is a
  one-directional pipe that stops early whenever its session is reaped or
  ffmpeg exits, and an HLS playlist that stops growing reads as complete. On
  that signal the player posted the full runtime as the position, which cleared
  the server's 95% threshold — and since the watched flag is deliberately
  sticky, one truncated stream marked a film watched for good and told Trakt so.
  `ended` now only counts as completion when the playhead really is at the end;
  short of that the true position is recorded and playback resumes from there,
  giving up after three failures at the same spot rather than looping. The
  player also no longer offers a growing stream's `duration` as the runtime —
  only direct play's own duration is the whole file — and the server clamps a
  reported position to the runtime so a resume point can't land past the last
  frame. Anything already mis-marked can be cleared with **Mark unwatched** on
  the item.
- On a touchscreen, the playback-info panel no longer buries the player
  controls. It had no size limit and no close affordance — touch has no `i` key
  to press — so opening it covered both control bars with no way back. It now
  carries a close button, scrolls internally, and is capped against the top
  bar's real height (it wraps to three rows on a narrow screen), remeasured on
  rotate. It stays a card rather than stretching corner to corner, and is only
  as tall as what it holds, so on a tablet it covers about a sixth of the screen
  instead of most of it; it fills the width only where the screen is too narrow
  for the card. The quality menu is bounded the same way, and the two panels
  take turns rather than stacking.
- Remux streams are no longer delivered as fast as the link allows. A `-c copy`
  remux was a disk-to-socket pipe braked only by the browser's buffer, measured
  here at over 200× real time; because every seek opens a fresh stream, seeking
  meant repeated line-rate bursts. On Wi-Fi that monopolises airtime for the
  duration, and a client whose DHCP lease came up for renewal during one could
  lose the lease and then struggle to reacquire it: the client's own requests
  are 802.11 unicast to the AP and so are ACKed and retried by the MAC layer,
  but the server's reply has to cross a downlink queue that the burst is busy
  filling, and if it is group-addressed it gets no ACK, no retry, the lowest
  basic rate, and a wait for the next DTIM. Streams now burst 30 seconds
  flat-out (so starting and seeking stay instant) and then settle to 4× real
  time,
  configurable in **Settings → Playback → Delivery speed**.
- Seeking no longer stacks transcode sessions. Each seek started a new session
  and left the old one for the idle reaper, so up to ~75 seconds of a second
  ffmpeg kept encoding after every seek, without bound. A new session now
  supersedes that viewer's previous session on the same file immediately.
- The web player no longer strands hls.js instances. Overwriting the live
  instance on seek, audio switch, or fallback left the old one polling a
  playlist that never ends, forever; each one also re-fetched segments from the
  session it was supposed to have left behind.
- Fixed a progress-timer leak on auto-next: each episode transition left the
  previous episode's 5-second progress poster running, so after N episodes N+1
  reported concurrently.
- `POST /api/v1/client-log` is rate-limited to 30 reports/minute. A browser
  looping on an error could otherwise flush the entire 2000-line log ring in
  seconds, erasing the history an operator opens the page to read. Suppressed
  reports are counted and reported rather than silently dropped.

## [0.1.0] — 2026-07-22

First numbered development milestone; no `v0.1.0` tag was cut. Everything
before this point was developed under the
placeholder version `0.0.1`, which never moved and never corresponded to a tag;
the entries below describe what that work amounts to rather than reconstructing
a hundred commits of history.

### Added

- **Media server.** Library scanning for movies and TV (filename parsing,
  incremental rescans, filesystem watching), TMDB and AniList metadata with
  cached artwork, and per-user watch state including resume positions.
- **Playback.** Direct play where the client can already decode the file,
  remux into a browser-friendly container where only the container is wrong,
  and HLS transcode where it isn't — chosen per client from advertised codec
  support. Hardware encode via NVENC, QuickSync, VA-API, and VideoToolbox, each
  validated by a test encode at startup so only paths that actually work get
  used.
- **Web app.** A single-file SPA that doubles as the admin UI: browse, search,
  a custom player overlay with subtitle and audio track selection, quality
  override, intro/credit skip, autoplay, and Chromecast/AirPlay handoff.
- **Plex compatibility.** A Plex Media Server API façade plus GDM discovery, so
  existing Plex clients connect directly without knowing plurx exists.
- **Clients.** Android and iOS applications alongside the web app.
- **Operations.** Container image with a hardware-transcode-capable ffmpeg,
  Prometheus metrics at `/metrics`, an in-process log buffer surfaced in
  settings, and health probes.

### Changed

- Version numbering is now real: the workspace carries a semantic version, the
  binary is stamped with the git commit it was built from, and `/api/v1/server`
  reports both. See [docs/RELEASING.md](docs/RELEASING.md).

[Unreleased]: https://github.com/pjunod/plurx/compare/v0.2.7...HEAD
[0.2.7]: https://github.com/pjunod/plurx/releases/tag/v0.2.7
