# PGS overlay Milestone 0 — feasibility evidence

**Status:** in progress; repository-owned parser and boundary prerequisites
reconciled, default-off Milestone 1 server producer and automated clients
complete; controlled production corpus and physical-device evidence pending ·
**Decision:** continue the bounded FFmpeg-to-SUP architecture, do not use the
candidate's direct MKV/M2TS path, and do not enable PGS overlay in production
yet · **Updated:** 2026-08-14

Companion to [PGS_OVERLAY_PLAN.md](PGS_OVERLAY_PLAN.md). This is the evidence
ledger for that plan's Milestone 0. It distinguishes what this branch proves
from what must still be observed on representative media and physical Apple and
Android hardware.

## 1. Review outcome

`libpgs` 0.6.0 is suitable only behind the bounded raw-SUP adapter in
[`crates/plurx-pgs`](../crates/plurx-pgs). It is not approved for direct parsing
of untrusted Matroska or M2TS files. The staged `plurxd` producer now depends on
that adapter, but its API capability and routes are default-off behind
`PLURX_PGS_OVERLAY`; this does not authorize production selection or client
rollout.

The exact release passes the initial license and dependency review. The source
audit found memory and malformed-input behavior that Plurx must not inherit
unchanged:

- the upstream RLE decoder sizes its output directly from untrusted 16-bit
  width and height values, which can request about 4 GiB of palette-index
  storage before RGBA expansion;
- that decoder treats an incomplete bitmap as transparent padding instead of
  reporting malformed input;
- the upstream display-set assembler retains segments until an END arrives,
  without a caller-provided segment or byte ceiling;
- the Matroska compressed-block path expands zlib with `read_to_end`, without
  a decoded-byte limit;
- the extractor retains a clone of every yielded display set unless history is
  explicitly disabled;
- a failed track-filter reopen returns the original unfiltered extractor,
  which is not a safe failure mode for a server selecting one requested track;
- the candidate models `0x00`, `0x40`, and `0x80` composition states but not
  the `0xC0` state recognized by FFmpeg and reported in some multi-clip input;
- the 0.6.0 source contains no fuzz target or property-test harness.

The adapter addresses the raw-SUP subset by opening one file identity for both
an allocation-free structural preflight and the parse, then verifying the same
content digest on both passes. Plurx owns display-set assembly and PCS parsing;
the candidate supplies the remaining bounded payload decoders. That boundary
supports the `0xC0` Epoch Continue state the candidate omits, unwraps the
32-bit PTS clock, validates display state before bitmap allocation, and uses a
strict Plurx-owned RLE decoder and normalizer. It deliberately does not wrap
the candidate's MKV/M2TS readers.

This is a conditional **continue** through server-contract review, not a
production go decision.

## 2. What landed in the laboratory

The `plurx-pgs` workspace crate remains the only raw-PGS interpretation
surface:

- `libpgs` is pinned to exactly `=0.6.0`;
- the staged server producer depends on it; clients never parse raw PGS;
- only raw SUP input is accepted;
- preflight rejects unknown segment types, truncated headers or payloads,
  incomplete display sets, ordinary backwards PTS, and configured byte/count
  excesses;
- one open file handle pins input identity across preflight and parse, while a
  digest comparison rejects in-place content changes between the two passes;
- the 32-bit 90 kHz timestamp is unwrapped across a jump larger than half its
  range; smaller backwards jumps remain malformed;
- Plurx-owned display-set and PCS parsing retains `0xC0` Epoch Continue in file
  order and flushes the preceding palette/object cache, matching FFmpeg;
- the adapter resolves palette and object reuse across display sets;
- Acquisition Point, Epoch Start, and Epoch Continue discard the preceding
  palette and object cache, matching FFmpeg's reference state transition;
- ODS fragments must form a complete object of exactly the declared length;
- strict RLE rejects zero-length runs, truncated escape forms, row overflow,
  extra rows, and incomplete images;
- authored window, crop, placement, canvas, and object bounds are validated;
- epoch state, retained objects, retained palettes, and decoded pixels are
  bounded;
- each normalized composition receives a stable SHA-256 fingerprint for
  comparison with future reference output;
- the `pgs-feasibility` command emits a machine-readable JSON measurement
  report.

The fingerprints describe normalized RGBA content, crop, placement, canvas,
and object order. They are laboratory evidence, not the proposed HTTP manifest
schema.

## 3. Reviewed limits

These are deliberately conservative Milestone 0 limits. Production-file
measurements may justify a change, but a reviewer must approve that change
before a server route uses it.

| Boundary | Laboratory limit |
|---|---:|
| Raw SUP source | 512 MiB |
| Display sets | 250,000 |
| Segments per display set | 512 |
| Encoded payload per display set | 64 MiB |
| Canvas | 4096 × 2160 |
| Canvas pixels | 8,847,360 |
| Objects per composition | 64 |
| One decoded RGBA object | 36 MiB |
| One encoded RLE object | 32 MiB |
| Objects retained in one epoch | 1,024 |
| Palette-index pixels retained in one epoch | 128 MiB |
| Palettes retained in one epoch | 64 |

Every arithmetic expansion at the adapter boundary uses checked integer math.
The preflight happens before `libpgs` can accumulate one display set.

## 4. Dependency and supply-chain assessment

The reviewed package is [`libpgs` 0.6.0 on
crates.io](https://crates.io/crates/libpgs/0.6.0), corresponding to upstream tag
[`v0.6.0`](https://github.com/matthane/libpgs/tree/v0.6.0) and peeled commit
`2f28c900b5b194d07ee222b2c7d8be9daea646d4`. The crate is dual-licensed
MIT or Apache-2.0. Its crates.io checksum, recorded in `Cargo.lock`, is
`4aa4709d0e4f48ab8d5915a56133aac020d9938fc5aa94eed505da5e96886cbd`.

The parser has one direct runtime dependency, `flate2`. The exact added
transitive tree is:

```text
libpgs 0.6.0
└── flate2 1.1.9
    ├── crc32fast 1.5.0
    │   └── cfg-if 1.0.4 (already present)
    └── miniz_oxide 0.8.9
        ├── adler2 2.0.1
        └── simd-adler32 0.3.10
```

The reviewed Rust source contains no `unsafe` block. It uses ordinary Rust
errors for malformed SUP headers and truncated segment payloads. The relevant
upstream implementation is visible in the exact-tagged
[SUP reader](https://github.com/matthane/libpgs/blob/v0.6.0/src/sup/stream.rs),
[display-set assembler](https://github.com/matthane/libpgs/blob/v0.6.0/src/pgs/display_set.rs),
[RLE decoder](https://github.com/matthane/libpgs/blob/v0.6.0/src/pgs/rle.rs),
and [Matroska decoder](https://github.com/matthane/libpgs/blob/v0.6.0/src/mkv/mod.rs).
The cache transition is cross-checked against FFmpeg's current
[`pgssubdec.c`](https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/pgssubdec.c),
which flushes cached objects and palettes for every non-normal composition
state.

Dependency-changing pull requests now run the official RustSec
[`audit-check`](https://github.com/rustsec/audit-check) action through
[`rust-audit.yml`](../.github/workflows/rust-audit.yml). Separate jobs scan the
shipped workspace and sanitizer-only fuzz lockfiles. The workflow has no
advisory ignores and must pass before this branch merges. This moves the scan
to the same clean Linux environment that evaluates the committed lockfiles
instead of depending on an untracked developer-machine installation. The first
workspace scan used advisory database revision
`6d7aef354b4144c1ede046034adfd00246d3b0c0` (updated 2026-08-04) and found
RUSTSEC-2026-0194 and RUSTSEC-2026-0195 in `quick-xml` 0.38.4. This branch
upgraded the direct dependency to the patched 0.41 line. The latest official
workspace, vendored-lock, and fuzz scans over these unchanged lockfiles passed
in GitHub Actions run `31422757657` on 2026-08-10 with advisory database
revision `e26c6017b1dc91480abe4c875a15f6eadebb72ad`: zero vulnerabilities and
no ignore list. The workspace scan separately reports the informational
`RUSTSEC-2025-0141` unmaintained notice for `bincode` 2.0.1; it is not a hidden
ignore or a vulnerability finding.

The dependency is young and lightly adopted. Exact pinning is therefore
mandatory. Any upgrade requires repeating the source audit, malformed corpus,
resource measurements, and normalized fingerprints before the pin changes.

## 5. Deterministic fixture result

The repository's [`scripts/mkpgs`](../scripts/mkpgs) fixture authors two visible
compositions and two explicit clears on a 1920 × 1080 canvas. The bounded
adapter produced:

| Measurement | Result |
|---|---:|
| SUP bytes | 2,504 |
| Segments | 16 |
| Display sets | 4 |
| Visible compositions | 2 |
| Clears | 2 |
| Palette definitions | 2 |
| Object definitions | 2 |
| Largest RGBA object | 432,000 bytes |
| Largest RLE object | 1,080 bytes |
| Peak retained palette-index pixels | 108,000 bytes |

Composition fingerprints, in source order:

```text
 1000 ms  e864dc16af5d7d257cdbe7f4645b808d9c105ebf8370f14e76d49e60b9b85e70
 7000 ms  375009aa13c9613c4fa6ca3f7ed7d92a11a1332f907b449d84fc95c7932447d9
 9000 ms  cc2be8c68dbacd13aad1329389ff9267305c26dd8a76e46169a66967c14ac3e2
15000 ms  375009aa13c9613c4fa6ca3f7ed7d92a11a1332f907b449d84fc95c7932447d9
```

The two clear compositions intentionally share one fingerprint. The two shown
objects differ because they use different object identifiers even though the
fixture geometry is the same; this keeps the composition fingerprint sensitive
to state reuse errors.

Twenty-two focused crate tests (nineteen unit tests and three boundary
integration tests) currently prove:

- normal show and clear normalization;
- palette update with object reuse;
- object update with palette reuse;
- cache invalidation at an Acquisition Point;
- cache invalidation at both Epoch Start and Epoch Continue;
- strict rejection of the candidate decoder's transparent-padding behavior;
- canvas rejection before bitmap decode;
- truncated display-set rejection;
- 32-bit PTS wrap unwrapping while an ordinary backwards jump is rejected;
- ordered handling of identical timestamps;
- preflight segment bounding before candidate assembly;
- fragmented-object reassembly before strict decode;
- multiple simultaneous objects with authored crop rectangles;
- owned parsing and normalization of the `0xC0` Epoch Continue composition
  state;
- one open input identity across structural preflight and parsing, even when the
  pathname is replaced;
- deterministic mutation of every fixture truncation and hundreds of
  single-byte state changes without a panic.

### 5.1 Malformed-input fuzz campaign

The repository now carries a sanitizer-backed cargo-fuzz target in
[`fuzz/`](../fuzz). It sends arbitrary files through the complete bounded path:
structural preflight · candidate SUP parsing · fragment assembly · strict RLE
decode · palette and composition normalization. Its reduced profile caps an
input at 1 MiB, one decoded object at 8 MiB, and retained object pixels at
16 MiB so a generated header cannot turn the campaign into an allocation test
of the host.

The committed corpus contains both the minimal malformed header seed and the
2,504-byte valid SUP emitted by `scripts/mkpgs 1920 1080`. The nightly workflow
runs that target for 15 minutes against the committed corpus, uploads any
reproducer under `fuzz/artifacts`, and fails after artifact publication when
libFuzzer reports a crash. This is the recurring safety net; a longer campaign
over controlled production seeds remains release evidence.

The 2026-08-04 campaign used cargo-fuzz 0.13.2, libfuzzer-sys 0.4.13, and
Rust nightly 2026-08-01 with address sanitizer. Seeded with the deterministic
2,504-byte SUP, it completed 306,124 executions in 61 seconds with no panic,
crash, timeout, sanitizer finding, or parser allocation failure. The final
coverage signal was 983 edges and 2,403 features across 124 retained inputs.

A first run with a 256 MiB process ceiling stopped at 6,306 executions because
libFuzzer plus address sanitizer reached 271 MiB. Its allocation report assigned
the two largest allocations to the libFuzzer driver, not the adapter. Repeating
the same corpus with a 1 GiB process ceiling completed and stabilized at 449 MiB
RSS. That distinction matters: sanitizer-process RSS is not the parser's
reviewed object or epoch limit.

This is a clean bounded campaign, not proof over every production authoring
pattern. Production SUP tracks must join the seed corpus after their controlled
extraction, and a longer campaign remains part of the Milestone 0 acceptance
record.

### 5.2 FFmpeg reference check

FFmpeg/FFprobe 8.1.2 independently recognizes the fixture as
`hdmv_pgs_subtitle`, reports a 1920 × 1080 canvas and a `1/90000` timebase, and
sees all 16 packets at the authored PTS values: 1, 7, 9, and 15 seconds.

Composited over a black 1920 × 1080 reference frame at 24 fps, FFmpeg reports
the nontransparent subtitle bounding box as `(440,885) 1040×90`. That matches
the fixture exactly: a 1200 × 90 object placed at `(360,885)`, with 80
transparent pixels on both horizontal edges.

FFmpeg rebases the first raw-SUP timestamp to output time zero. Its output
transitions are:

| Output frame | Relative time | State |
|---:|---:|---|
| 0 | 0 ms | Clear |
| 1 | 41.7 ms | First object visible |
| 144 | 6,000 ms | Clear |
| 192 | 8,000 ms | Second object visible |
| 336 | 14,000 ms | Clear |

The six-second display durations and two-second gap match the adapter after the
one-second raw-stream origin is removed. The first object appears one 24 fps
frame after FFmpeg's rebased zero rather than on frame 0. That offset is now a
timing-spike question; it must not be silently baked into the manifest.

The reference rawvideo MD5 is `1aec365ed729807384b42199138ea085` for a visible
frame and `9b2c4088acb2c896d5d79adbe4b893ac` for a clear black frame. This proves
packet timing and visible geometry for the deterministic fixture. The committed
colored-cue sample also matches FFmpeg's BT.709 palette conversion exactly;
full-track pixel comparison against controlled production media remains open.

### 5.3 Production cold-scan attempt

A title-free measurement attempt copied subtitle index 1 from production file
5559 to raw SUP inside the running server container. It was intentionally
stopped after 179 seconds, when the incomplete temporary SUP had reached
2,097,152 bytes. No partial file was passed to the parser and no media artifact
was retained.

This is not a completed extraction benchmark, but it is enough to reject any
design that waits for whole-file FFmpeg demux before starting video or reporting
that PGS is being prepared. The fallback requires detached warming, a deadline,
single-flight coordination, and visible progress. It also strengthens the case
for measuring sparse/missing Matroska subtitle cues before accepting a direct
container reader.

File 5698 was not scanned during this attempt because it had an unrelated active
production playback transcode. Feasibility work must not contend with a viewer
for the source merely to fill an evidence table.

## 6. Reproduce the parser evidence

From the repository root:

```sh
scripts/mkpgs 1920 1080 target/pgs-m0-fixture.sup
cargo run -p plurx-pgs --bin pgs-feasibility -- target/pgs-m0-fixture.sup
cargo test -p plurx-pgs
cargo clippy -p plurx-pgs --all-targets -- -D warnings
ffprobe -v error -show_streams -show_packets -select_streams s:0 \
  target/pgs-m0-fixture.sup
mkdir -p /tmp/plurx-pgs-corpus
cp target/pgs-m0-fixture.sup /tmp/plurx-pgs-corpus/mkpgs.sup
cd fuzz
cargo fuzz run inspect_sup /tmp/plurx-pgs-corpus -- \
  -max_total_time=60 -rss_limit_mb=1024 -timeout=5 -max_len=1048576
```

The generated SUP and JSON under `target/` remain disposable laboratory
artifacts. One deterministic copy of that SUP is committed only as a fuzz seed;
it contains no operator media. The fuzz sub-workspace pins its required nightly
toolchain in `fuzz/rust-toolchain.toml`; `cargo-fuzz` remains an explicitly
installed developer tool rather than a shipped dependency.

## 7. Milestone evidence matrix

| Required evidence | State | Evidence or next action |
|---|---|---|
| Exact dependency and security assessment | Pass | The latest applicable official action, run `31422757657`, scanned the unchanged workspace, vendored, and fuzz dependency graphs at advisory-database revision `e26c6017b1dc91480abe4c875a15f6eadebb72ad`; it reported zero vulnerabilities and no ignores. The workspace-only `RUSTSEC-2025-0141` bincode notice remains informational. |
| Bounded Plurx-owned adapter | Pass for raw SUP | Exact pin, preflight, strict normalizer, limits, and focused tests are in this branch. |
| Production files 5559 and 5698 | Partial | File 5559 cold demux was stopped at 179 seconds and 2 MiB incomplete output; file 5698 was left untouched during active playback. Repeat off-hours with progress and a deadline. |
| Malformed-input campaign | Partial pass + nightly gate | 306,124 sanitizer-backed executions completed cleanly over the deterministic seed. The valid 2,504-byte generated SUP is now committed beside the malformed-header seed, and the bounded nightly campaign preserves crash artifacts and fails on a reproducer. Add title-free controlled production seeds and rerun the longer campaign. |
| FFmpeg or Media3 reference comparison | Partial pass | FFmpeg 8.1.2 matches deterministic packet times and visible bounds; HD canvases now use BT.709 and a colored-cue RGBA sample matches the committed FFmpeg fixture. Repeat on production tracks. |
| Apple timed-overlay prototype | Automated pass; physical pending | The iOS/tvOS client uses `AVSynchronizedLayer` over `AVPlayerLayer.videoRect`; shared simulator tests cover non-zero `base_ms`, seek-window reconciliation, authored-canvas layout, selection/off, and player-item replacement. Run the timing matrix on supported physical devices. |
| Android timed-overlay prototype | Pending | Exercise `SurfaceView`, tunneling, source-time mapping, and timeline epochs on a physical supported device. |
| Dolby Vision/HDR preservation | Pending | Record device diagnostics and display-mode evidence before and after PGS selection. |
| Seek, pause, rate, item replacement | Automated partial | Source/item conversion, seek reconciliation, synchronized item timing, and item replacement are covered on both simulators. Record physical onset/clear error across play, pause, rate, and repeated scrubbing. |
| PiP, AirPlay, casting, external output | Policy pass; physical pending | Apple blocks PiP and external playback while PGS overlay is active and explains why, without falling back to burn-in. Confirm the limitation on physical devices; Android remains pending. |
| Cold extraction and cache measurements | Partial | Synthetic SUP numbers and the bounded 5559 attempt are recorded. Measure completed time, bytes read, peak RSS, cue count, object deduplication, and PNG size off-hours. |

## 8. Milestone 1 server evidence

The server implementation is complete behind `PLURX_PGS_OVERLAY=1` and remains
off by default. The final schema is in
[PLAYBACK.md](PLAYBACK.md#pgs-overlay-server-contract--staged-and-default-off).

| Required evidence | State | Evidence |
|---|---|---|
| Complete composition normalization | Pass | `normalize_sup` emits complete positioned RGBA snapshots and explicit clear events without exposing `libpgs` types. The owned display-set/PCS adapter retains Epoch Continue, applies its cache reset, and unwraps the 32-bit PTS clock before normalization. |
| Malformed/limit behavior | Pass | Existing structural/RLE/state tests, stable-input digest checks, and aggregate normalized-RGBA bounds protect the adapter; the 306,124-execution ASan corpus remains the parser baseline. |
| Golden manifest/object behavior | Pass for deterministic fixture | Complete-state coalescing, last-state-wins handling at an identical source millisecond, a media-duration clamp when the final clear is absent, authored clear gaps, 1080p authored-canvas geometry over 4K video, content-addressed PNG deduplication, object existence, and content hashes are validated before publication. Representative production goldens remain part of physical acceptance. |
| Authentication and codec typing | Pass | Route regression proves both manifest and object authentication, PGS-only 415 behavior, generation matching, and immutable object headers. |
| Concurrency, durability, and atomicity | Pass | One producer owns a generation, peer requests receive preparation state, files and directories are synced before the whole-directory rename, and a torn or evicted generation is removed and re-prepared instead of remaining a permanent failure. |
| Deadline and retry suppression | Pass | Ten-minute producer bound, two-minute bounded negative memo, and focused injected-timeout regression. |
| Capacity and eviction | Pass | Two concurrent generation producers, 256 MiB per-track output cap, and independent 2 GiB/128-generation access-marked LRU budget. |
| Invalidation | Pass | Generation digest includes file id, track index, size, mtime, schema, and extractor version; focused regression changes track and source identity. |
| Old-client compatibility | Pass | `overlay` is omitted while disabled and on non-PGS tracks; existing `text` and `native` fields retain their meaning. |
| Raw-SUP demux boundary | Fixture pass; production deadline fail | A generated SUP round-trips through the production FFmpeg mapping and bounded adapter. On 2026-08-09, cold demux of track 0 from a 25,016,940,779-byte NAS-resident 4K HDR10 episode hit the real 600-second deadline at 600.08 seconds before producing a SUP file. The 256 MiB cap held by construction, but cue and PNG measurements remain unavailable because parser admission never began. |

The server implementation deliberately does not claim physical Apple/Android
presentation, Dolby Vision preservation, PiP/external-output inclusion, or
production-media resource acceptance. The automated Apple evidence below
narrows that list without satisfying the physical release gates.

## 8.1 Milestone 2 Apple automated evidence

The Apple client consumes only authenticated `pgs-v1` manifests and PNG
objects; raw PGS never crosses into the app. It validates file and track
identity, generation shape, monotonic non-overlapping cues, canvas/object
bounds, and exact content-addressed object paths before rendering. Fetching is
bounded to a five-second lookbehind and 90-second lookahead with a 96 MiB
decoded-image cache.

The player attaches an `AVSynchronizedLayer` to the current `AVPlayerItem` and
maps authored canvas coordinates into `AVPlayerLayer.videoRect`. XCTest passes
on both iOS and tvOS simulators for 1080p-on-4K, 4:3 letterboxing, anamorphic
canvas mapping, non-zero `base_ms`, item replacement, selection switching, and
Off. The route-policy regression proves a recognized PGS overlay sends neither
the native-subtitle nor burn-in session field and does not reopen direct video.

The implementation intentionally disables Picture in Picture and external
playback while PGS overlay is selected because the custom application layer is
not part of those output surfaces. A visible notice is preferable to silently
burning and converting Dolby Vision/HDR to SDR. This is automated behavior
evidence only: physical iPhone, iPad, and Apple TV timing and display-mode
verification remain required.

## 9. Parser-prerequisite verdicts

The seven prerequisites tracked by issue 168 have the following disposition:

| # | Verdict | Evidence and remaining action |
|---:|---|---|
| 1 | Partial | The committed corpus now includes a valid generated SUP as well as the malformed seed, and the nightly sanitizer gate runs for 15 minutes. Private production media cannot enter git: obtain title-free, sanitized SUP extracts covering files 5559 and 5698 plus a representative multi-clip title, then run the same 15-minute-or-longer campaign and attach its summary to the parent issue. |
| 2 | Done | Focused tests cover palette update with object reuse, object update with palette reuse, simultaneous cropped objects, fragmented ODS sequences, identical timestamps, the FFmpeg-matched BT.709 colored cue, 1080p authored canvas over 4K video, and a missing final clear clamped to media duration. |
| 3 | Done | The adapter unwraps only a backwards raw-PTS jump larger than half of the 32-bit clock and still rejects ordinary backwards time. Display sets at the same source millisecond remain in file order; manifest compilation keeps the last complete state and emits no duplicate cue start. Both native clients already require monotonically ordered, non-overlapping cues, so the manifest contract remains compatible. |
| 4 | Partial (parser closed) | The Plurx-owned PCS adapter accepts `0xC0`, measures it, resets the epoch cache, and has acceptance and stale-cache regressions. Whether the state occurs in representative multi-clip media remains part of the controlled production corpus request in item 1. |
| 5 | Done | The staged producer still invokes FFmpeg for one selected PGS track and admits only its bounded raw-SUP output. Neither the HTTP module nor parser accepts MKV or M2TS input directly. |
| 6 | Done | The producer opens the staged SUP once, uses that handle for preflight and parse, and compares SHA-256 digests across both passes. Both halves carry a regression: `already_open_input_keeps_one_identity_after_its_path_is_replaced` replaces the pathname after open and proves parsing stays on the original identity, and `source_rewritten_under_the_pinned_handle_is_rejected` rewrites the pinned inode between the two passes and proves the digest comparison rejects it. The second was added after a revert proof showed the digest check was unpinned by any test. |
| 7 | Done | Official RustSec action run `31422757657` used database revision `e26c6017b1dc91480abe4c875a15f6eadebb72ad`, found zero vulnerabilities across workspace, vendored, and fuzz scans, and used no advisory ignores. This branch does not change any Cargo manifest or lockfile from that audited dependency graph. |

Items 1 and 4 share one external evidence gap rather than an additional parser
change: a controlled, sanitized production corpus must exercise authoring
patterns that cannot be inferred from the generated fixture. Record that result
on the parent issue before the feature gate is considered for enablement.

## 10. Go/no-go criteria for production enablement

Approve Milestone 1 only if all of the following are attached to this ledger or
its implementation PR:

- production files 5559 and 5698 stay within reviewed limits or justify a
  reviewed limit change;
- reference renders match normalized palette, pixels, crop, placement, clear,
  and timing behavior;
- fuzzing finds no crash, panic, unbounded allocation, or accepted malformed
  state at the adapter boundary;
- cold extraction and output size are operationally acceptable;
- physical Apple and Android tests show PGS while the selected video range and
  Dolby Vision/HDR presentation remain unchanged;
- output-mode limitations are measured and disclosed;
- `PLURX_PGS_OVERLAY` remains off in production until this evidence is accepted;
  no client treats absent capability as permission to burn HDR/DV.

Until then, the correct result for PGS on active HDR/Dolby Vision playback
remains a visible refusal to burn, preserving the video presentation rather
than silently switching to SDR.

The repository-owned parser and raw-SUP boundary prerequisites are therefore
closed except for the explicitly identified production-corpus measurement.
That result does not satisfy the production-media, physical-device, HDR/Dolby
Vision, or operational cold-extraction gates above, and it does not authorize
enabling `PLURX_PGS_OVERLAY` by default.

## 11. Versioning

The fuzz harness itself adds no runtime dependency to the server or either
client and does not enable a production playback path. The first RustSec gate
did expose two advisories in the server's existing XML dependency, so this
branch upgrades `quick-xml` to the patched 0.41 line. That shipped-runtime
change advanced the shared marketing/workspace version to 0.2.3, Apple build
to 23, and Android version code to 12. The staged server contract changes the
shipped daemon and advances the shared marketing/workspace version to 0.2.4,
Apple build to 24, and Android version code to 13. Later client implementation
branches must re-read `main` and advance their affected release counters again.
The Apple Milestone 2 branch does so at shared version 0.2.5, Apple build 25,
and Android version code 14; the Android counter remains coordinated even
though its renderer lands in a separate branch and PR. The Android Milestone 3
branch advances the coordinated release to 0.2.6, Apple build 26, and Android
version code 15.
