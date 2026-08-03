# Media badges — the play menu tells the truth about HDR and Dolby Vision

**Status:** **landed** @ `0fa0aff` (M1, server) · `3ea2321` (M2, web) ·
`7f4950f` (M3, Android) · `a6643f7` (M4, Apple), on branch
`clients/remediation`. M5 is not built. **Device verification is
outstanding**: §11's matrix has not been run on any hardware, and the Apple
client has never been compiled — there is no macOS in the environment M4 was
written in. · **Executes:** Paul's 2026-08-02 ask ("make the
play menu media info indicative of both what the media is supposed to have
and what is successfully being used by the client … explicitly if HDR or
Dolby Vision is there, and if it's being rendered") · **Written:** 2026-08-02
against working tree `97176881e` on branch `codex/fix-dolby-vision-quality`

Claims this plan got wrong are corrected in place and marked **Corrected**,
with the commit that disproved them, rather than quietly rewritten: the plan
is a record of what was believed as well as an instruction, and the code is
what won. Every correction below was re-verified against the tree, not
against the commit message that reported it.

Read §2 (the truth model) and §3 (the contract) before writing any code —
every milestone hangs off the one new wire field defined there. Work
milestone by milestone, in order: M1 (server) unblocks M2–M4 (one per
client), which are independent of each other. M5 is optional polish. The
standing instruction: **if a step seems to require changing playback
decision policy, the DV strip/copy pipeline, or client capability
reporting, stop and flag it instead** — this feature *reports* what those
systems do; it must never alter what they do. File/line references were
true at `97176881e`; the tree moves fast (another agent works this branch
natively), so re-verify each anchor at build time.

## 1. Objective

Today every media-info badge row — the chips in the play overlay and on the
detail screens — is built from the *source file's probe*: a Dolby Vision
disc remux shows "DV P7" even while Chrome is actually watching a
tone-mapped 1080p SDR transcode of it. The badge answers "what is this
file?" when the viewer during playback is asking "what am I *getting*?"

After this plan, the dynamic-range badge answers both at once:

- The badge always names what the source carries ("DV P7", "HDR10+",
  "HDR10", "HLG") — what the media is supposed to have.
- **Lit** (full colour, as today) means that grade is actually being
  delivered to and rendered by this client, this session.
- **Dimmed with an arrow** ("DV P7 → HDR10", "HDR10 → SDR") means the
  source grade is not what's on screen, and the suffix names what is.
- The playback-info panel gains an explicit "Dynamic range" row saying the
  same thing in words, with the reason.

Scope is the dynamic-range badge only. The same mechanism generalises to
resolution (4K source → 1080p rung) and audio (Atmos → AAC stereo), and the
design keeps that door open (§9), but this plan ships HDR/DV.

## 2. The truth model — source, delivered, rendered

Three layers, each with exactly one owner. Conflating them is how the
current badges came to lie.

| Layer | Question | Owner | Where it lives today |
|---|---|---|---|
| Source | What does the file carry? | server probe | `MediaFile.hdr` / `hdr_format` (scan/probe.rs `detect_hdr`) |
| Delivered | What grade is in the bytes this session sends? | server decision + session | derivable, **not yet on the wire** — the gap this plan closes |
| Rendered | Is the display actually showing that grade? | client | display/decoder APIs, per platform (§2.2) |

### 2.1 Delivered — the server already knows, it just doesn't say

Every fact needed is already in `plurx-core`. The pipeline has exactly
three deliveries, and their dynamic-range outcomes are total:

```
 method?
   │
   ├─ direct_play ───────────────────────▶ source's grade, untouched
   │
   ├─ remux / copy session
   │     │
   │     ├─ preserve_dolby_vision ──────▶ dolby_vision  (dvh1 tag, RPUs kept)
   │     │
   │     ├─ DV source, strip ───────────▶ base layer:
   │     │                                 hdr_format "(HLG-compatible)" → hlg
   │     │                                 else                          → hdr10
   │     │
   │     └─ non-DV source ──────────────▶ source's grade (video is copied)
   │
   └─ transcode ────────────────────────▶ sdr, always
```

Why "transcode → sdr, always" is safe to hard-code: every transcode encodes
H.264 8-bit (`transcode/encoder.rs` — libx264 / h264_nvenc / h264_qsv /
h264_vaapi / h264_videotoolbox, no 10-bit path), and an HDR source goes
through the tone-map graph (`TranscodeOptions.tone_map`, default
`ToneMap::Zscale`). The `ToneMap::None` escape hatch still emits 8-bit with
no HDR signalling — washed out, but SDR on the wire — so "sdr" stays the
honest answer even then.

Why the strip branch can trust the compat marker: `DvHandling::Strip` is
only reachable through `has_compatible_dv_base(file)` (playback/mod.rs), so
a stripped remux always has "(HDR10-compatible)" or "(HLG-compatible)" in
`hdr_format`. A DV source without a compatible base re-encodes instead —
which is the `sdr` branch.

**Corrected `0fa0aff`: the flowchart above has one hole, and it is in that
last sentence.** `decide_forced(Force::Original)` means "no video
re-encode", so it copies a DV source the client cannot decode whatever
`dv_handling` said — including a Profile 5 source, whose base layer is not
an HDR10 grade in any sense. The helper answers `"hdr10"` there and
over-claims. The over-claim is deliberately left rather than guessed at:
narrowing it needs `dv_strippable`, a fact about the *server* rather than
about the delivery the helper's signature describes, and the stream in
question is one no client that asked for it can play — the error path
rescues it into a transcode, whose session then reports `"sdr"` for what
the viewer actually got. The reasoning is on the helper's doc comment so
the next reader meets it there and not here.

### 2.2 Rendered — what each client can honestly claim

Delivered bits are necessary but not sufficient: an HDR10 direct-play on an
SDR monitor is delivered HDR and rendered SDR. Each client combines the
server's delivered value with the strongest *local* signal it has. Do not
invent stronger signals than these — where a platform has no public API,
the honest formula is server-delivered ∧ display-capable.

| Client | Display capable of the grade? | Decoder confirmation (when available) |
|---|---|---|
| Web | `matchMedia("(dynamic-range: high)")`, read live at render time (the window can move between monitors) | none — browsers expose nothing per-stream; the server value stands |
| Android | `Display.getHdrCapabilities().supportedHdrTypes` — `HDR_TYPE_HDR10`/`HLG`/`DOLBY_VISION` (the probe `Caps.displayIsHdr` already wraps this) | ExoPlayer `player.videoFormat`: `colorInfo.colorTransfer` = ST2084/HLG confirms PQ/HLG in the decoded stream; `sampleMimeType == MimeTypes.VIDEO_DOLBY_VISION` confirms the DV decoder engaged. Already read by the info panel (`videoFormatSummary`, PlayerScreen.kt ~1131) |
| Apple | `AVPlayer.eligibleForHDRPlayback` (Caps.swift already uses it as the device's HDR/DV gate) | none public for HLS variants — do NOT try to introspect the active variant; the dvh1-tagged master + eligibility is the contract (`hevc_copy_tag` doc comment: dvh1 or AVPlayer never engages its DV pipeline) |

The rendered verdict, uniformly:

```
rendered_grade = delivered_grade
if delivered_grade is DV/HDR and the display cannot show it → rendered_grade = sdr
(Android only) if decoder confirmation is present and contradicts, trust the decoder
```

### 2.3 The three badge states

One badge, whose text always starts from the source grade. State is a pure
function `(source_grade, rendered_grade)`:

| State | Condition | Visual | Example |
|---|---|---|---|
| **lit** | rendered == source grade | today's full-colour chip (gold DV / teal HDR) | `DV P7` |
| **different grade** | rendered ≠ source grade | source half dimmed; arrow suffix stays lit and names what's on screen | `DV P7 → HDR10`, `HDR10 → SDR` |
| **source-only** | no active session (detail screens) | today's rendering, unchanged (M5 adds capability dimming) | `DV P7` |

The `DV P7` in that last column is the **web** chip. The profile number is
a web-only refinement — `hdrChip` regexes it out of `hdr_format`, and
Android and Apple both label the chip `DV` from the coarse grade alone.
Read every `DV Px` in this document as "the source-grade mark, whatever
this client spells it"; the arrow suffix is the same vocabulary
everywhere.

DV delivered as HDR10 separates capability from function: the unavailable
source half dims, while `→ HDR10` remains fully lit because HDR is working.
The same split applies to SDR, where the bright suffix truthfully reports the
rendered result rather than implying that the source capability is active.
Accessibility labels spell it out: "Dolby Vision, playing as HDR10". The
hover/long-press detail (web `title`, info panels elsewhere) carries the
server's reason string, which already says *why* ("Dolby Vision metadata
removed for this device; compatible HDR base kept").

## 3. Contract — exact interfaces at `97176881e`

Re-verify every signature against the file before editing; this branch is
also being worked natively by another agent.

### 3.1 What exists today

**Source grades** — `crates/plurx-core/src/scan/probe.rs::detect_hdr`
returns `"dolby_vision" | "hdr10" | "hlg" | None`; `detect_hdr_format`
returns the rich label ("Dolby Vision · Profile 7 (HDR10-compatible)",
"HDR10+", …). Both already reach every client as `MediaFileDto.hdr` /
`hdr_format` and `SourceSummary.hdr` / `hdr_format`.

**The decision** — `crates/plurx-core/src/playback/mod.rs`:

```rust
pub struct Decision {
    pub method: PlaybackMethod,          // DirectPlay | Remux | Transcode
    pub reasons: Vec<String>,
    pub transcode_audio: bool,
    pub preserve_dolby_vision: bool,
    pub container: &'static str,
}
pub fn decide(file: &MediaFile, profile: &DeviceProfile, dv_strippable: bool) -> Decision
pub fn decide_forced(file, profile, force: Force, dv_strippable) -> Decision
fn dv_handling(file, profile, dv_strippable) -> DvHandling   // None|Strip|Reencode
fn has_compatible_dv_base(file: &MediaFile) -> bool
```

**The HTTP shapes** — `crates/plurxd/src/http/stream.rs`:
`DecisionResponse { file_id, #[serde(flatten)] decision, play_url,
delivery: DeliveryPlan, source: SourceSummary, audio, subtitles, markers,
…, ladder }` — the flatten means a field added to `Decision` lands on the
wire with no plurxd change. `crates/plurxd/src/http/hls.rs`:
`StartResponse { session_id, playlist_url, duration_ms, start_seconds,
encoder, vod, ladder }`, produced by `create()`, which already loads the
source file (`state.store.get_file(id)`) and builds
`SessionKind::Copy { aac, preserve_dolby_vision } | SessionKind::Transcode
{ height }`.

**Copy-path DV mechanics** (evidence for §2.1, do not modify) —
`crates/plurx-core/src/transcode/mod.rs::hevc_copy_bsf_for_client`
(preserve → keep RPUs; strip → `dovi_rpu=strip=1,…`) and
`hevc_copy_tag` (preserve → `dvh1`, else `hvc1`).

**Client badge code:**

- Web — `crates/plurxd/src/web/index.html`: `hdrChip(f)` (~line 2043,
  source-only, returns `{cls:'dv'|'hdr', text, full}`), `specBadges(f)`
  (detail page), `playerFactBadges()` (~2080, reads `PLAYER.source`),
  rendered into `.pifacts` by the player-panel renderer (~3984). Player
  state: `PLAYER = { method, decidedMethod, preserveDolbyVision, reasons,
  source, copyHls, sessionId, … }` (~2892). Session attach:
  `attachSession(video, PLAYER, info, startSec)`; transitions:
  `startCopyHls` (~2990), `startTranscodeFallback` (~3754), the quality
  menu, and the subtitle-burn path (all funnel through `openSession`).
  Capability probe: `PLAY_CAPS` closure (~2715) — computes `hdrDisplay`
  from `matchMedia("(dynamic-range: high)")` but does not expose it.
  Info overlay Output section: ~5010–5040. CSS: `.vbadge`, `.vbadge.hdr`
  (teal), `.vbadge.dv` (gold) at ~612–623.
- Android — `ui/components/MediaFacts.kt`: `MediaFact(kind, label,
  accessibilityLabel)`, `playerMediaFacts(file, audio)`,
  `detailMediaFacts(file)`, `dynamicRangeFact(file)` (source-only),
  `MediaFactChip` (uniform 0.78-alpha styling). Rendered in
  `player/PlayerScreen.kt` `Controls(mediaFacts = playerMediaFacts(...))`
  (~660, ~809) and `ui/DetailScreen.kt` (~451). `data/Models.kt`:
  `Decision` **has no** `preserve_dolby_vision` and `HlsStart` **has no**
  `vod`/`ladder` — this plan adds only what badges need (§6).
  **Corrected `7f4950f`: all three exist now.** They were true absences
  when this was written; CLIENTS-REMEDIATION-PLAN §5.4/§5.5/§5.6 added them
  in the same commit as M3, which is why §6.1's "resist back-filling them"
  is moot — that milestone landed with them already there.
  `data/Caps.kt::displayIsHdr(context)` is private. Info panel:
  `PlayerInfo` → `videoFormatSummary(format)` already maps
  `colorInfo.colorTransfer` to "HDR10 / PQ"/"HLG" (~1131).
- Apple — `Sources/PlayerView.swift`: `PlayerMetadataBadge { kind, symbol,
  mark, accessibilityLabel }`, static `playbackBadges(source:audio:)`
  (~597, source-only), rendered by `playbackFacts` (~543) from
  `controller.decision?.source`. `Sources/PlayerController.swift`:
  everything funnels through `open(decision:at:)` (~278) — `direct` plays
  the raw URL, otherwise `CreateSessionRequest(copy:…,
  preserveDolbyVision: copy ? decision.preserveDolbyVision : nil)`.
  `Sources/Models.swift`: `Decision`/`Delivery`/`CreateSessionRequest`
  already carry `preserveDolbyVision`; `HlsStart` does not yet carry a
  delivered field. `Sources/Caps.swift`: `AVPlayer.eligibleForHDRPlayback`
  gates both `hdr` and `dv` caps. Stats panel: `sourceRows`/`outputRows`
  (~1094–1120). `DetailView.itemMetadataBadges` (~822) has **no**
  dynamic-range badge at all today.

### 3.2 The new wire contract (the entire server surface of this feature)

One value, spelled identically in two places, both additive:

```jsonc
// GET /api/v1/files/{id}/decision            (DecisionResponse, flattened)
{ "method": "remux", "preserve_dolby_vision": false, …,
  "delivered_dynamic_range": "hdr10" }        // NEW: what THIS plan delivers

// POST /api/v1/files/{id}/hls/sessions       (StartResponse)
{ "session_id": "…", "encoder": "…", "vod": false, …,
  "delivered_dynamic_range": "sdr" }          // NEW: what THIS session delivers
```

Values: `"dolby_vision" | "hdr10" | "hlg" | "sdr"` — the same vocabulary as
`MediaFile.hdr` plus `"sdr"`, so clients compare source vs delivered with
string equality. Rules:

- The decision's value describes the delivery plan as decided. The session
  response's value describes the session actually created, and **overrides**
  the decision's value the moment a session attaches (a burn or a manual
  rung forces a transcode the decision never promised).
- Direct play has no session; the decision's value stands for the whole
  playback.
- On `StartResponse` the field is `Option<String>`/nullable: a session
  created for a file the store can't find mid-request (source lookup
  returned `None`) omits it, and clients keep whatever they had.
- Old clients never read the field; new clients treat an absent field as
  "unknown" and fall back to source-only badges. Nothing breaks in either
  direction.

### 3.3 The one shared helper

Both call sites compute the value through a single public function in
`plurx-core::playback`, so the decision and the session can never disagree:

```rust
/// The dynamic range of the bytes a delivery actually puts on the wire.
/// Total over the three delivery methods; see MEDIA-BADGES-PLAN.md §2.1.
pub fn delivered_dynamic_range(
    file: &MediaFile,
    method: PlaybackMethod,
    preserve_dolby_vision: bool,
) -> &'static str {
    if method == PlaybackMethod::Transcode {
        return "sdr";                       // every transcode is H.264 8-bit
    }
    match file.hdr.as_deref() {
        Some("dolby_vision") if preserve_dolby_vision => "dolby_vision",
        Some("dolby_vision") =>
            // Strip is only reachable with a compatible base (§2.1).
            if file.hdr_format.as_deref()
                .is_some_and(|f| f.contains("HLG-compatible")) { "hlg" }
            else { "hdr10" },
        Some("hdr10") => "hdr10",
        Some("hlg") => "hlg",
        _ => "sdr",                          // SDR source, or never probed
    }
}
```

## 4. M1 — server: put `delivered_dynamic_range` on the wire

All in `crates/plurx-core/src/playback/mod.rs` and
`crates/plurxd/src/http/hls.rs`; `stream.rs` needs no edit beyond what the
flatten gives for free.

1. Add the helper from §3.3, `pub`, next to `is_dolby_vision`.
2. Add `pub delivered_dynamic_range: &'static str` to `Decision`, and
   populate it at the end of `decide()` and in **both arms of
   `decide_forced()` that construct a `Decision`** (`Force::Transcode` →
   `"sdr"` via the helper; `Force::Original` computes with its own method +
   preserve flag). The serde derive on `Decision` puts it on
   `DecisionResponse` via the existing flatten.
   **Corrected `0fa0aff`:** this step said "**all three** arms".
   `decide_forced` does have three arms, but `Force::Auto` delegates
   straight to `decide` and constructs nothing — there are only two
   `Decision` literals to populate, and `decide`'s own is the third.
3. `hls.rs::create()`: compute from the already-loaded `source` and the
   `SessionKind` it just built —
   `Copy { preserve_dolby_vision, .. }` → helper with
   `PlaybackMethod::Remux`; `Transcode { .. }` → `"sdr"` — and add
   `delivered_dynamic_range: Option<&'static str>` (serialize, skip or
   null when `source` was `None`) to `StartResponse`. The deprecated GET
   `start()` bridge inherits it by delegating to `create()` — no edit.
4. Tests, in playback/mod.rs's existing test module style (they narrate the
   scenario; match that voice):
   - DV P7 HDR10-compatible + DV-capable client → direct play, delivered
     `dolby_vision`.
   - Same file + Chrome-shaped caps + strippable server → remux, delivered
     `hdr10`; with "(HLG-compatible)" in `hdr_format` → `hlg`.
   - Same file + non-strippable server → transcode, delivered `sdr`.
   - HDR10 source remuxed (container mismatch) → `hdr10`; transcoded
     (SDR profile) → `sdr`; SDR source anywhere → `sdr`.
   - `decide_forced(Force::Original)` on a DV file the client can't take →
     remux, delivered `hdr10` (strip honoured), and `Force::Transcode` →
     `sdr` even on a direct-playable DV file.
   - In plurxd: a copy session with `preserve_dolby_vision: true` answers
     `dolby_vision`; the same create with `copy: false` answers `sdr`
     (extend the existing hls tests around create()).

**Acceptance:** `cargo test --workspace --no-fail-fast` green, and:

```bash
curl -s "$S/api/v1/files/$DV_FILE/decision?$CHROME_CAPS" | jq .delivered_dynamic_range
# → "hdr10"   (item 6041/6045 with hdr=1&dv=0 caps, dovi_rpu present)
```

## 5. M2 — web player

The state, the pure function, the chip, the panel row — in that order.

1. **Expose the display signal.** Add `hdrDisplay` to the object `PLAY_CAPS`
   returns, and read it *live* where the badge renders via a tiny
   `displayIsHdr()` helper (`matchMedia` re-queried per render — monitors
   change; the probe's cached copy is only the boot-time answer the server
   got).
2. **Track delivered.** `PLAYER` gains `deliveredRange` — seeded from
   `decision.delivered_dynamic_range` at player construction (~2892), then
   overwritten with `info.delivered_dynamic_range` inside `attachSession`
   (one site catches every session: transcode start, `startCopyHls`,
   `startTranscodeFallback`, burn, rung/audio switches). Falsy → keep the
   previous value (§3.2).
3. **The pure function**, next to `hdrChip`:

   ```js
   // (sourceHdr, sourceFormat, deliveredRange, displayHdr) →
   //   {cls, text, full, off}   or null when the source is SDR
   function dynamicRangeBadge(f, delivered, displayHdr){ … }
   ```

   Behaviour: source grade names the chip exactly as `hdrChip` does today;
   `rendered = (delivered in {dolby_vision,hdr10,hlg} && !displayHdr)
   ? 'sdr' : delivered`; `delivered == null` → source-only (today's chip);
   rendered equal to the source's coarse grade → lit; otherwise
   `off: true` and `text += " → " + label(rendered)` ("HDR10", "HLG",
   "SDR"), with `full` spelling it out ("Dolby Vision — playing as HDR10
   (Dolby Vision removed for this browser)"). Reuse `PLAYER.reasons` for
   the parenthetical when one mentions "Dolby Vision" or "HDR".
4. **CSS.** Add `.vbadge.off` — keep the border, drop the tinted
   background, ~0.45 opacity on the icon and source part. The arrow suffix
   rides inside the same chip (no second chip; the row already wraps on
   phones — see the f07dd96 lesson).
5. **Wire it in.** `playerFactBadges()` calls `dynamicRangeBadge(PLAYER.
   source, PLAYER.deliveredRange, displayIsHdr())` instead of
   `hdrChip(PLAYER.source)`. Detail-page `specBadges` stays source-only
   (M5 owns pre-play dimming).
6. **Info panel row.** In the Output section (~5010), before "Dropped":
   `Dynamic range · Dolby Vision (rendering)` / `HDR10 — Dolby Vision
   removed for this browser` / `SDR (tone-mapped from HDR10)` — build the
   sentence from the same pure function's parts.
7. **Tests, per the Rust-gate-is-blind rule.** The gate cannot see this
   file (`include_str!`): (a) extract the largest `<script>` and
   `node --check` it; (b) extract `dynamicRangeBadge` by regex and drive a
   node case table: {DV P7 + delivered hdr10 + HDR display → off, arrow
   HDR10} · {DV P8 + dolby_vision + HDR display → lit} · {hdr10 + hdr10 +
   SDR display → off, arrow SDR} · {hdr10 + sdr → off} · {sdr source →
   null} · {delivered undefined → today's chip}.

**Chrome today, and why it's a feature:** the live DV bug
(docs and PERF notes: Chrome refuses even the stripped remux with an MSE
parser error, then the player rescues into a transcode) means Chrome on the
two DV films will show **"DV P7 → SDR"**, not "→ HDR10". That is the badge
working — it reports the fallback the session actually took. Do not
"fix" the badge to hide it; when the parser bug is fixed the arrow retreats
to "→ HDR10" (and to lit on Safari) with zero badge changes.

**Corrected `5d5deff`: the rescue this paragraph assumes did not exist on
the transport it describes.** A media failure under hls.js never reaches
the `<video>` element — hls.js owns the MediaSource and consumes it — so a
refused copy stream ended at a toast and a dead player, not at a transcode.
The rescue the progressive path always had now exists on the hls.js path
too (copy verdicts only, once per item, media/codec fatals only), which is
what makes "DV Px → SDR" the badge the viewer actually sees rather than the
badge on a stream that stopped. The same commit stopped routing 10-bit
HEVC through MSE on an 8-bit decoder's yes, which is the black-picture case
that produced no error at all.

**Acceptance:** node syntax check + case table green; manually: Safari on
an HDR Mac playing 6045 shows `DV P8` lit; Chrome shows `DV P8 → SDR`
today; the info panel row matches the chip in both.

## 6. M3 — Android

Android reality check first: this client sends **no `dv`/`dvprofile` caps**
(known punch-list item, CLIENTS-CODE-REVIEW §10), so the server strips DV
for every Android session. The badge will therefore show `DV → HDR10` (or
`→ SDR` on a transcode) on every DV file until that punch-list item lands —
**correct behaviour, not a defect of this feature**. Do not implement DV
caps here; the badge lights up by itself when that ships.

**Corrected `7f4950f`: Android DV caps are implemented.** M3 and
CLIENTS-REMEDIATION-PLAN §5.1 landed in the same commit, so the "do not
implement them here" fence never had a second agent on the other side of
it — see §9's non-goal, corrected the same way. `Caps.kt` now probes
`MediaCodecList` for `video/dolby-vision`, maps the profile constants, and
claims the intersection with the panel's `HdrCapabilities`, never claiming
profile 7. The badge therefore lights on a device that claims a profile the
file carries, and the paragraph above describes only what a device that
claims nothing still sees.

1. **Models** (`data/Models.kt`): add `val delivered_dynamic_range: String?
   = null` to `Decision` and to `HlsStart`. Nothing else — resist back-
   filling `vod`/`ladder`/`preserve_dolby_vision`; they're other work.
2. **State** (`player/Controller.kt`): mirror `activeMode`/`deliveryMode`
   with a `deliveredRange` (Compose `mutableStateOf<String?>`), seeded from
   the decision when the plan starts and overwritten from every `HlsStart`
   the controller opens (session create, fallbacks, burns, audio/rung
   reopens — wherever `activeMode` is updated today).
3. **Display + decoder signals**: in `data/Caps.kt`, widen `displayIsHdr`
   to an internal `displayHdrTypes(context): Set<Int>` (same deprecated-API
   suppression, returning `supportedHdrTypes`), keeping the existing
   boolean as a wrapper so the caps query is untouched. In the player,
   compute the rendered grade:

   ```kotlin
   // delivered from server; format = controller.player.videoFormat
   fun renderedRange(delivered: String?, format: Format?, hdrTypes: Set<Int>): String?
   // DV: delivered=="dolby_vision" needs HDR_TYPE_DOLBY_VISION in hdrTypes
   //     AND (format==null || format.sampleMimeType==VIDEO_DOLBY_VISION)
   // hdr10/hlg: need a non-empty hdrTypes AND
   //     (colorInfo==null || colorTransfer is ST2084/HLG respectively)
   // contradiction from a non-null decoder signal wins over the server value
   ```

4. **The chip** (`ui/components/MediaFacts.kt`): `MediaFact` gains
   `val state: FactState = FactState.Source` (`Source | Active |
   Downgraded`) and an optional `activeLabel: String?` for the arrow.
   `MediaFactChip` renders `Downgraded` at reduced alpha with
   `"$label → $activeLabel"` and keeps `Source`/`Active` as today
   (`Active` may brighten to full alpha — the current 0.78 chip becomes
   the lit look). `playerMediaFacts` gains the delivered/rendered inputs
   and sets the dynamic-range fact's state; `detailMediaFacts` keeps
   emitting `Source` (M5 owns detail dimming). Update `MediaFactsTest`
   with the same case table as the web's (§5.7).
5. **Wire-in** (`player/PlayerScreen.kt`): pass
   `controller.deliveredRange`, `controller.player.videoFormat`, and the
   display types into `playerMediaFacts` at the `Controls(...)` call site;
   recomposition follows from the state reads. Add an explicit
   "Dynamic range" row to `PlayerInfo` beside the existing
   `videoFormatSummary` line, same sentences as the web row.

**Acceptance:** `./gradlew :app:testDebugUnitTest` green (MediaFactsTest +
a `renderedRange` table + ModelContractTest decoding the new field); on an
Android TV with an HDR panel, 6041 plays with the chip reading
`DV → HDR10` (copy session) and the info row naming the strip; force a
rung → chip flips to `DV → SDR` live.

**Corrected:** the acceptance said `DV P7 → HDR10`. The Android mark is
`"DV"` — `dynamicRangeFact` labels the chip from the *coarse* grade and has
never carried a profile number. Only the web chip does, because `hdrChip`
regexes the profile out of `hdr_format`; the arrow suffix is the same
vocabulary on all three clients.

## 7. M4 — Apple

The only client that can render DV end-to-end today (sends `dvprofile=5,8`,
receives a `dvh1`-tagged copy). Everything funnels through
`open(decision:at:)`, which makes the state change one edit.

1. **Models** (`Models.swift`): `Decision` and `HlsStart` gain
   `var deliveredDynamicRange: String?` (the decoder's key strategy already
   maps `delivered_dynamic_range`).
2. **State** (`PlayerController.swift`): `@Published private(set) var
   deliveredRange: String?`. In `open`: the `direct` branch sets it from
   `decision.deliveredDynamicRange`; the session branch sets it from
   `hls.deliveredDynamicRange` after `createHlsSession` returns (nil →
   leave the decision's value standing, per §3.2). It resets with each
   reopen — burns, rung picks, and the compat-retry path all pass through
   here, so no other site exists.
3. **Badges** (`PlayerView.swift`): extend the static, already-tested
   builder — `playbackBadges(source:audio:delivered:displayHDR:)` with
   defaults (`delivered: String? = nil, displayHDR: Bool = true`) so the
   existing tests keep compiling, then update them. `PlayerMetadataBadge`
   gains `var renderedMark: String? = nil` plus `var dimmed: Bool = false`;
   the dynamic-range badge computes
   rendered = `displayHDR ? delivered : "sdr"` (with
   `AVPlayer.eligibleForHDRPlayback` passed from the view), compares
   against the source grade, and on mismatch keeps `mark = "DV"`, sets
   `renderedMark = "HDR10"`, dims only the icon/source group to ~0.45, and
   leaves `→ HDR10` fully lit inside the existing capsule. The spelled-out
   `accessibilityLabel` remains "Dolby Vision, playing as HDR10". Do not
   reach for `UIScreen.currentEDRHeadroom` or `AVDisplayManager` — eligibility
   is the documented, stable signal; headroom polling is listed in §9 as a
   non-goal.
4. **Stats panel**: add a "Dynamic range" row to `outputRows` from
   `controller.deliveredRange` + eligibility, sentence-style like the web's.
5. **Detail parity** (`DetailView.swift`): `itemMetadataBadges` today has
   no dynamic-range badge at all — Android and web detail pages do. Add a
   source-only badge (kind `.dynamicRange`, symbol `sparkles`, mark
   `"DV"`/`"HDR"` exactly as `playbackBadges` labels it) after the codec
   badge, and update the AppleClientTests assertions that pin the exact
   badge arrays (~727).

**Acceptance:** `xcodegen && xcodebuild test` on Paul's box (the cloud
sandbox cannot build Apple targets — keep every new rule inside the static
funcs so the test target carries the logic); on an Apple TV 4K with a DV
display, 6045 (P8) shows `DV` lit in the play overlay; with the TV's
Dolby Vision output disabled (Settings → Video and Audio → Format), the
same badge reads `DV → HDR10`-or-`→ SDR` per what
`eligibleForHDRPlayback` then reports.

**Corrected `a6643f7`:** this acceptance said `DV P8`. The Apple mark has
never carried a profile number — `DynamicRange.sourceMark` answers `"DV"`
or `"HDR"` and nothing else, so the prose overstated what this client
renders. Same for Android (§6); the web chip is the only one that spells
the profile, and it gets it from a regex over `hdr_format`, not from
anything this plan added.

## 8. M5 (optional) — capability dimming on the detail screens

Pre-play, there is no session, but each client already knows its own caps —
so the detail badge can honestly warn "this device will not render this
grade" before the viewer commits. Web: `PLAY_CAPS.dv`/`hdr` +
`displayIsHdr()` feed the same `dynamicRangeBadge` with a *predicted*
delivered value (DV source + `dv:0` → the strip/transcode prediction is
"hdr10-at-best" — dim with "→ HDR10?"); Android: `Caps` +
`displayHdrTypes`; Apple: `Caps.dolbyVisionSupported`. Keep the arrow
suffixed with `?` on detail screens — it is a prediction, the server may
still decide otherwise (forced transcode, non-strippable ffmpeg). Ship only
if M1–M4 land clean; it touches three more surfaces for a softer claim.

## 9. Non-goals — guardrails

- **Do not change any decision, strip, tone-map, or caps-probe behaviour.**
  This feature is a reporter. If a truth-table case seems to need a
  pipeline change, stop and flag (orientation paragraph).
- **Do not implement Android DV caps** (`dv`/`dvprofile` in Caps.kt) — it's
  an existing punch-list item being worked on this branch by another agent;
  doing it here collides. The badge is designed to be correct without it.
  **Corrected `7f4950f`: they are implemented** — M3 and
  CLIENTS-REMEDIATION-PLAN §5.1 landed together, so there was no other
  agent to collide with. The guardrail's *point* survives: the badge was
  built to be correct without them, and it did not change when they
  arrived.
- **Do not add audio or resolution states in this pass.** The `FactState`/
  `dimmed`/`off` mechanics are deliberately generic; wiring Atmos→AAC and
  4K→rung truthfulness is a follow-up with its own truth table (audio
  needs the per-session `aac`/downmix facts surfaced first).
- **No new endpoints, no polling.** The value rides the two responses every
  client already consumes. In particular, do not poll `hls/{s}/status` for
  it — status reports throughput, not format.
- **No private/fragile display introspection**: no `UIScreen.currentEDRHeadroom`
  loops, no `AVDisplayManager` criteria writes, no Chrome
  `video-dynamic-range` experiments beyond the one media query. The
  documented signals in §2.2 are the contract.
- **Do not hide the Chrome DV→SDR truth** (§5) while the MSE parser bug
  stands — the badge saying so is the point of the badge.

## 10. Process — branch, gates, and doc upkeep

- **Branch first**: `git branch --show-current` before assuming anything —
  as of 2026-08-02 the checkout sits on `codex/fix-dolby-vision-quality`
  (another agent's active branch; main is an ancestor and this arc merges
  through it). Coordinate: land this work on its own branch off the same
  line unless told otherwise.
- **The gate**: `make check` (fmt + clippy `-D warnings` + tests); run
  `cargo test` with `--no-fail-fast` or plurx-core failures hide behind
  plurxd's. The toolchain pin is 1.97.1 (`rust-toolchain.toml`).
- **The gate is blind to the web player**: for any `index.html` change,
  extract the big `<script>` → `node --check`, and run the §5.7 case
  table. If the badge CSS changes row height, measure with headless
  chromium (`executablePath: /opt/pw-browsers/chromium`) against the
  shipped `<style>` — the info panel's column layout was measured, not
  eyeballed, and regressions there are silent.
- **Tests that touch tempdirs**: reproduce the macOS symlink drift before
  handoff (`mkdir /tmp/realtmp && ln -sfn /tmp/realtmp /tmp/linktmp &&
  TMPDIR=/tmp/linktmp cargo test --workspace`).
- **Docs move in the same commit**: add the badge-state table (§2.3) to
  `docs/PLAYBACK.md`, note the two new wire fields wherever
  `DecisionResponse`/`StartResponse` are documented, a line in
  `docs/FEATURES.md`, and a `CHANGELOG.md` entry. Mark this plan's Status
  line "landed @ <sha>" when it merges — **done**, and it took four shas
  rather than one because the milestones landed per client.

## 11. Acceptance matrix — the two films that started all of this

Both are 4K HEVC 10-bit MKV disc remuxes with HDR10-compatible DV bases:
item **6041** (Godfather, DV **P7**, TrueHD) and item **6045** (Shawshank,
DV **P8**, DTS). Every cell is the badge text expected in the play
overlay after M1–M4; ✱ marks cells that flip when known open items land.

| Client | Expected badge | Why |
|---|---|---|
| Safari, HDR Mac | 6045: `DV P8` lit · 6041: `DV P7 → HDR10` | web probe asks only profiles 5/8 (`dvCan("05.06")`/`("08.07")`), so P8 preserves and P7 strips to its base |
| Chrome, HDR display | `DV Px → SDR` ✱ | strip-remux refused (open MSE parser bug) → transcode rescue; becomes `→ HDR10` when the parser bug is fixed |
| Chrome, SDR display | `DV Px → SDR` | server tone-maps (hdr=0 caps) |
| Android TV, HDR panel | 6041: `DV → HDR10` · 6045 on a DV-claiming panel: `DV` lit | `Caps.kt` now sends `dvprofile` for the profiles the decoder lists and the panel shows, never 7 — so P8 preserves where the device claims it and P7 always strips |
| Apple TV 4K, DV output on | 6045: `DV` lit · 6041: `DV → HDR10` | Caps.swift sends `dvprofile=5,8`; P8 → dvh1 copy engages the DV pipeline, P7 is not claimed → strip |
| Apple TV 4K, forced 1080p rung | `DV → SDR` | transcode session; badge follows the session, not the decision |
| Any client, HDR10 (non-DV) source, HDR display, remux/direct | `HDR10` lit | video copied untouched |
| Any client, HDR10 source, transcode for bitrate/subs burn | `HDR10 → SDR` | every transcode tone-maps to H.264 8-bit |

**Corrected:** the Android and Apple rows spelled the mark `DV Px`. Only
the web chip carries a profile number (`hdrChip` regexes it out of
`hdr_format`); Android's `dynamicRangeFact` and Apple's
`DynamicRange.sourceMark` both answer `"DV"`. The Safari and Chrome rows
are the web player and keep theirs. The Android row's ✱ is also spent —
`7f4950f` shipped the dv-caps item it was waiting on (§6).

If a cell disagrees with the shipped behaviour, the bug is in the feature,
not the matrix — the matrix restates §2.1, which restates the pipeline.
**None of these cells has been observed.** Every one of them is a
prediction until someone runs the two films on the hardware named in the
row; the Status line says so, and so does this.
