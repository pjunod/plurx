# WO-05 — HLS flow control: global-cap release + the EVENT-playlist mutation

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P1 (task 1 code fix; task 3 is the highest-value experiment for the iPad stalls)**

## Context

The #91 fix commit restored hysteresis (180 s hold / 150 s release), added `hold_reason` telemetry, and pinned the suspend guard — flow control is healthy again. Two structural items remain, plus observability that the next investigation will need. Ground truth to respect: the "producer suspension starves the client" theory was **refuted** (published media stays fetchable; don't chase release thresholds again).

## Tasks

1. **Global-cap release can be structurally unreachable — change its basis.**
   `crates/plurxd/src/transcode.rs:646-651`: the global hold compares `global_bytes` (= `live_bytes` = `index.total_bytes()`, `:1286` — total on-disk session bytes) against `limit/2` when suspended. But GC never prunes inside `RETENTION_SECS = 180 s` behind each session's frontier, so total bytes have a hard floor of Σ(per-session retention) that **no client fetching can drain** — unlike per-session `ahead.bytes`, which drains to zero. Whenever Σ(retention floors) > limit/2, every producer is SIGSTOPed forever; clients drain their buffers and stall; their polls keep sessions alive so the idle reaper never fires. Arithmetic at defaults: three-to-four concurrent 60-80 Mbps 4K remuxes carry ~1.35-1.8 GB un-prunable floor each → 4 × 1.35 GB > 4 GiB release line. This is the one true deadlock shape left.
   Fix (pick one, smallest first): (a) release the **global** hold on Σ`ahead_bytes` (drainable) instead of total bytes; (b) set global release to `limit − headroom` where headroom ≥ max session ahead-window, instead of half; (c) longer term, refuse admission when projected retention floors exceed the release line.
   Acceptance: unit test constructing N synthetic session indexes whose retention floors sum above `limit/2` — after all `ahead` drains to zero, `ahead_hold(…, currently_suspended=true)` must return `None` under the new rule (it returns `Some(Global)` today — write the test first to prove the deadlock, then fix).

2. **First-class flow-control observability.**
   Suspend/resume are `debug!`-only (`transcode.rs:4628,4637`); the 0.7→12.2 pairs/min flapping regression was only recoverable from debug logs, and the reverted thresholds can't be confirmed from the activity page. Add: per-session `suspend_count` (atomic, surfaced in `SessionInfo` + web panel next to `hold_reason`); a one-shot `info!` when a session's served playlist first slides (session id, first retained index, wall time since start); idle age + last-request kind in the reap log (`:4740`).
   Acceptance: activity page shows suspend transitions; grep-able first-slide line in server logs.

3. **The EVENT→sliding-live mutation — decide, and run the experiment.**
   Both producers write `#EXT-X-PLAYLIST-TYPE:EVENT`; `served_live_playlist` (`transcode.rs:516` area) passes it through until the first prune (~2-4 min in, when `fetched_end` crosses 180 s), then the **same URL** drops the EVENT tag and starts advancing MEDIA-SEQUENCE. RFC 8216 §6.2.1: an EVENT playlist "MUST NOT change or delete any part of the Playlist file (it MAY append)". AVPlayer derives seekable ranges and reload policy from the declared type; mid-play the seekable start jumps from 0 to frontier−180 s. First prune at 2-4 min + stall-driven reopens restarting the clock reproduces a **periodic 2-5 min stall without any threshold being wrong** — this is the best-fitting untested hypothesis for the iPad freezes.
   Proposed fix: serve the typeless sliding shape **from the first response** for uncached sessions (never mutate mid-stream; stripping tags before any client loads the playlist violates nothing), plus `#EXT-X-START:TIME-OFFSET=0` so AVPlayer doesn't jump to the live edge. Seek already reopens sessions, so EVENT's scrub-back affordance isn't load-bearing.
   Experiment (device, one iPad): add a settings-gated variant serving the typeless shape; play a long title on one iPad; compare stall cadence vs a control run. Meanwhile correlate existing stall timestamps against the new first-slide log line from task 2.
   Acceptance: decision recorded in docs/PLAYBACK.md; if adopted, a served-playlist unit test asserting shape stability across the prune boundary (same headers before/after).

4. **P2 — pin overshoot.** Between evaluations, production is bounded only by tick×readrate (+120 s observed at 20×). One unit test pinning max overshoot at the default cadence keeps the next tuning honest.

5. **P2 — progressive-remux start latency.** #56 made `remux()` await `probe_media_origin` (5 s timeout) before first byte (`http/stream.rs:1398`) for every progressive open/seek, including clients that never read the header. On cold NAS that's up to +5 s to first byte. Cap the probe at ~1 s for the progressive path or race it against ffmpeg startup. Acceptance: first-byte bench with an artificially delayed probe.

## Don't

- Don't touch the 180/150 thresholds; don't reintroduce the identity release.
- Don't re-raise: producer-suspension starvation (refuted), copy-path release arithmetic, `resume_below` vacuity (fixed), the missing segment-completion caller (now honestly documented — a completion hook on the copy path is a nice-to-have, not this WO).
