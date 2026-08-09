# WO-08 — PGS overlay follow-ups

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P1 (tasks 1-4), P2 (rest) — feature is default-off (`PLURX_PGS_OVERLAY`), so nothing here is a production fire**

## Context

The staged PGS overlay (#59/#60/#62/#63/#64) has an excellent server contract (content-addressed generations, byte-budgeted artifact class with claim/complete/LRU, bounded decode with checked arithmetic, 422/415/503/409 taxonomy, token-free relative manifest URLs) and two honestly different client sync designs (Apple `AVSynchronizedLayer`; Android event-driven boundary scheduler). The gaps are safety-net items and two product decisions the accepted plan explicitly deferred to Paul.

## Tasks

1. **iOS auto-PiP fires with the overlay active — subtitles silently vanish.**
   Manual PiP is blocked (`PlayerController.swift:1411-1416` + notice), but `canStartPictureInPictureAutomaticallyFromInline = true` is set unconditionally (`PlayerSurface.swift:49`) and `PlayerView.swift:612` passes `allowsPictureInPicture: !lifecycle.isTearingDown` without consulting `pgsOverlayIsActive`. Swipe-home during DV+PGS → system PiP shows video with no subtitles, no notice. (The M0 ledger's "Apple blocks PiP while overlay is active" claim is overstated.)
   Fix: gate both on overlay activity (toggle the automatic flag on overlay selection; or clear the overlay selection with the existing notice when PiP engages).
   Acceptance: XCTest on the gating predicate; device check that backgrounding doesn't enter PiP (or shows the notice).

2. **Wire the fuzzer into CI.** `fuzz/fuzz_targets/inspect_sup.rs` covers the right surface (preflight → SUP parse → fragment assembly → RLE → normalization) but runs in **no workflow** — the only recorded campaign is a 61-second single-seed developer run, on a parser whose input is attacker-influenced media. Add a bounded nightly step (e.g. `cargo +nightly fuzz run inspect_sup -- -max_total_time=900`) with the corpus committed under `fuzz/corpus/` and failures uploaded as artifacts. Acceptance: nightly log shows executions > 0; a seeded crash reproduces red.

3. **Publication durability + self-heal.** `pgs_overlay.rs` renames generations into place with no fsync of files or parent dir (`:575, :644, :359`); after power loss a generation can hold zero-length PNGs/manifest, and `prepare` returns `Ready` on metadata existence alone (`:225`) — a torn generation is served forever (client shows a failed-subtitle notice until source mtime changes). Fix: fsync staged files + dir before rename, and on serve-time read/validation failure, delete the generation and fall back to re-prepare. Acceptance: inject a torn generation → next request re-prepares instead of 500/permanent-fail.

4. **Treat 503/202 as retryable in both clients.** Third concurrent cold extraction → capacity 503; both clients treat any non-200/202 as terminal for that selection (`AndroidPGSOverlay.kt:274-282`; `PlurxAPI.swift:227-229` → `.failed`) while capacity frees in seconds and both already own a 10-minute poll loop. Fix: retry 503 (and keep polling on 202) inside the prepare deadline. Acceptance: unit test — 503 → 202 → 200 yields Ready.

5. **Pin the revert-safe behaviors** (WO-04's class, found live here): (a) no test pins the gate default — flipping `PLURX_PGS_OVERLAY` to default-on stays green; add one asserting unset-env ⇒ routes 404. (b) Apple's overlay window refresh from the periodic observer (`PlayerController.swift:1751`) — deleting the line stays green while overlays die ~70 s after every seek; add a test through the refresh path. (c) `prune()` call sites (`pgs_overlay.rs:287,303`) are untested — dropping them = unbounded cache; add a call-site test.

6. **Color matrix decision.** `ycrcb_to_rgba` hard-codes BT.601 (`crates/plurx-pgs/src/lib.rs:1087-1095`); HD Blu-ray PGS is typically authored BT.709 — colored cues (signage, karaoke) shift hue vs a hardware player. Pick matrix by canvas size (≥720p → BT.709) or record a measured decision; commit an RGBA fixture comparison against ffmpeg's render of one colored production cue.

7. **P2 hygiene:** bound the demux stage (ffmpeg can write an unbounded `track.sup` into staging before the parser cap applies — disk-fill bounded only by the 600 s deadline; add `-fs` or a staging-size watchdog); LRU prune can evict a generation under an active viewer (`prune` after completion has no pinning — consider serve-time re-prepare on manifest miss, cheap given task 3); ETag-only caching (no `If-None-Match`/`Range` handling on PNG serves — nice-to-have).

## Decisions for Paul (the accepted plan's riders — flag, don't just implement)

- **Server-side HDR-burn guard:** the server still honors `subtitle_burn` codec-blind, so pre-guard clients (physical Apple TV on an old build) can still burn DV to SDR. Add the server-side refusal now, or accept until the overlay exits staging?
- **One-tap user-requested SDR burn:** the HDR-guard notice is still a dead end on all three clients — the plan recommended offering an explicit burn from the notice. Build it, or drop the idea?

## Don't

- Don't rework the artifact class, the manifest schema, or the client sync designs — reviewed sound (Apple's `isRemovedOnCompletion` coupling to seek-rebuilds deserves a comment, nothing more).
- Don't re-raise Android PiP: the plan expected overlay-visible-in-PiP, the code deliberately forbids PiP while the overlay is active pending hardware verification — that conservative deviation is fine; just test it on a device eventually.
