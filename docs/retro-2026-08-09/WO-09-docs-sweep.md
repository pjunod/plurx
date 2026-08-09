# WO-09 — Docs sweep (one commit)

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P2 — single docs-only commit (rides the docs-only CI lane)**

## Context

A full docs-coherence audit found the tree in better shape than any prior sweep — the #91 fix commit corrected OPERATIONS/PERF-PLAN/CHANGELOG claims to match code, and most historically-stale lines are fixed. What remains is one build-number sweep, missing CHANGELOG entries for the week's two biggest arcs, and STATUS/ROADMAP not admitting clustering is underway. Everything below was verified stale at the pinned sha; quoted line numbers are at that sha.

## The commit, file by file

1. `clients/apple/README.md:15` — "build `37`" → "build `38`".
2. `docs/APPLE-CLIENT-PARITY.md` — `:12` build 37 → 38; `:20` "build 37 has not reached TestFlight" → 38; `:55` now-column: credit the shipped #86 stall beacon ("one-shot stall/recovery beacon — delivery method, film position, stall_ms — through the bounded client log"), narrow remaining-work to sub-threshold stall reporting (see WO-06 task 1).
3. `docs/APPLE-NATIVE-SUBTITLES-PLAN.md` — `:361, :365, :471, :658` "build 37" → 38 (leave `:381` "37 or higher").
4. `clients/android/README.md:22-23` — "build `17`" → "build `18`" (versionCode is 18; keep the build-17-added-offline sentence as history, append "build 18 carries the playback-compatibility fallback fix").
5. `docs/STATUS.html` — footer `:277`: "reviewed against `origin/main` @ `e8a910f`", drop "plus the PR #91 review response"; header sub `:96`: append the #92 Skip Credits fix + build 38. **Phase 4 section `:206-216`**: currently "0 / 5 items ○ todo" — mark executing: "CLUSTERING-PLAN M0–M1c merged (identity, one-voter hiqlite proof, store-contract parity, replicated auth, replicated catalogue/media + local search, root-identity fences, `make cluster-check`) · M1d in review · daemon activation pending". Tile `:102` "M4 clustering next" → "clustering underway"; `:175` same. Add a line for this retrospective (docs/RETRO-REVIEW-2026-08-09.md) wherever review deliverables are listed.
6. `docs/ROADMAP.md:121` — "## Phase 4 — HA for real (IN PROGRESS)" + one line naming M0–M1c merged / M1d next / activation pending.
7. `docs/CLUSTERING-PLAN.md:3-5` — status → "M0–M1c merged; M1d in review"; `:123` — reword `enc_keys` (that hiqlite field is feature-gated OUT of the shipped build — `default-features = false`; say secrets are the two mode-0600 files, and `enc_keys` only returns if M3 enables backups/dashboard).
8. `CHANGELOG.md` `[Unreleased]` → Added — two missing headline entries: **clustering** ("replicated store proven end-to-end: hiqlite one-/three-voter backends replicate users, auth, settings, catalogue and search behind the unchanged Store trait; `make cluster-check` drills voter loss and incompatible-voter refusal; new admin endpoints `POST /api/v1/libraries/{id}/root-identity/reset` and `POST /api/v1/system/search-index/rebuild`; SQLite remains the default — no operator change yet") and **pgs-v1** ("staged, default-off `PLURX_PGS_OVERLAY`: authenticated PGS manifest/PNG overlay path drawing bitmap subtitles over unmodified DV/HDR/SDR video on Apple and Android; not a release claim until device acceptance").
9. `docs/CHEATSHEET.md` — env table: add `PLURX_SCAN_PRUNE_PERCENT` (default 10; 0 disables; max % of known files one scan may delete). API table: add the two new admin endpoints (root-identity/reset — "accept a verified replaced mount"; search-index/rebuild). Add one pointer line to the offline quota settings in OPERATIONS.md.
10. `README.md` — `:285` Phase 4 "[ ]" → "[~] … clustering M0–M1c merged behind the Store trait; single-node SQLite still the default"; `:293` Phase 5 "[ ]" → "[~] … Android and Apple clients working; Tizen/webOS/Roku not started"; `:17` soften "not yet wired up" → "replicated store backends merged, not yet activated".
11. `docs/PGS-OVERLAY-REVIEW-ASSESSMENT.md:477` — "Current `origin/main` is…" → "At this review's 2026-08-04 baseline, `origin/main` was…" (it names build 19 / versionCode 9 as "current").
12. `crates/plurxd/src/web/index.html:4490, :5955, :6194` — `console.warn("[cinemarr] …")` → `[cinema]` (brand/BRAND.md:29 retires the name). *Code file: this one takes the change out of the docs-only lane — either accept the full CI run or split it into its own commit.*

## Also in this commit (optional, Paul's call)

- The ten untracked `docs/PR*-REVIEW.md` pre-merge review files currently sitting in the working tree — commit them (they're the evidence trail this retro verified against) or delete them. Don't leave them untracked.
- `docs/ANDROID-CLIENT-PARITY.md:86` — still says session `start_seconds` retains the source timeline; stale after #58 (media_origin_ms / header is the anchor now, `start_seconds` the fallback).

## Acceptance

`rg -n "build .3[67]." docs clients | grep -v CHANGELOG` returns only historical references; `make check` green (history/operations contracts untouched); if WO-04 task 5's version-string contract lands, it goes green with this sweep.

## Don't

- Don't touch `docs/ARCHITECTURE.md:88-97` — verified accurate against merged reality (the coalescer text correctly describes M1d as pending).
- Don't "fix" OPERATIONS/PERF-PLAN flow-control text — corrected by #91's fix commit and verified current.
