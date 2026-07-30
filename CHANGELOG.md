# Changelog

All notable changes to plurx are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and plurx uses
[semantic versioning](https://semver.org/) under the 0.x rules described in
[docs/RELEASING.md](docs/RELEASING.md): while the major number is 0, a **minor**
bump may break compatibility and a **patch** bump never does.

## [Unreleased]

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

### Added

- Playback stats: native HLS (Safari) sessions now attribute hitches to
  playlist segment boundaries, the "fps rendered" figure counts frames that
  actually reached the screen rather than Safari's decode-ahead, and the
  overlay reports the realized playback rate whenever it deviates from what
  was requested.

## [0.2.0] — 2026-07-30

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

First numbered release. Everything before this point was developed under the
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

[Unreleased]: https://github.com/pjunod/plurx/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/pjunod/plurx/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/pjunod/plurx/releases/tag/v0.1.0
