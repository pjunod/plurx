# PR #94 review — persist Apple artwork across app launches

**Reviewed:** 2026-08-09 · head `3102069` (substantive commit `04c47e5`,
merge-base = `origin/main` @ `74825a1`) · 3 files, +443/−35, Swift-only ·
**Verdict: APPROVE WITH CHANGES** — the disk-cache actor is correct and
discriminatingly tested; fix the failure classification and add the
CHANGELOG entry before merge, and take the three follow-ups seriously.

Companion to the PR#79–#92 review docs in this folder. Method at the end;
Swift cannot compile in the review sandbox, so client-side claims are
verified by reading plus hand-derived test math, and a device pass for GPT
is included.

---

## Summary

The change replaces the memory-only `AuthImageCache` with a three-layer
design: size-keyed `NSCache` (300 entries, now carrying `storedAt`), a new
`AuthImageDiskCache` actor (source-keyed original bytes, SHA-256 filenames,
sidecar JSON metadata, 256 MiB LRU, 7-day fresh / 30-day stale), and a new
`AuthImageDownloadCoordinator` actor that coalesces same-URL downloads.
`AuthImage.load()` paints local hits first and refreshes stale ones in
place. `project.yml` bumps build 38→39.

Every claim in the PR body checks out against the tree: the 7-day window
mirrors a real server header (`crates/plurxd/src/http/images.rs:49`,
`public, max-age=604800`), `clear()` is wired at both sign-out paths
(`AppModel.swift:309`) and server switch (`:332`), and the origin-isolated /
provider-URL-shared `sourceKey` split is deliberate and tested.

## Fix before merge

**1. Every non-2xx is classified terminal and deletes the cached copy**
(`AuthImage.swift:225–236`, acted on at `:253`). `.terminalFailure` fires
for *any* status outside 200–299 and removes both the memory entry and the
disk bytes. But the server launders every artwork read error into 404 —
`images.rs:38–40` maps all `tokio::fs::read` failures (fd exhaustion under
transcode load, transient IO) to `NotFound` — and 5xx/429/502 are transient
by definition. So the exact conditions the 30-day stale window exists to
survive can instead bulk-purge it: a scroll during a server hiccup deletes
still-valid posters that were readable offline a second earlier. Base had
the same status check but nothing to destroy; this PR turns it into cache
loss. Fix is two lines: terminal = 401/403/404/410, everything else
transient. (Server follow-up, out of scope here: stop laundering non-ENOENT
IO errors into 404.)

**2. No CHANGELOG entry.** House convention holds for client changes —
#86, #87, #89, #91, and #92 all added entries; this user-visible behavior
change (artwork survives relaunches) ships without one. One `### Added`
line under `[Unreleased]`.

## Should fix — follow-ups, filed with reasons

**3. Downloads are no longer cancellable, and visible cells queue behind
scrolled-past ones.** Base checked `Task.isCancelled` in the retry loop and
ran inside the view task, so `.task(id:)` teardown cancelled the HTTP
request. Now the work runs in an unstructured `Task` inside the coordinator
(`:306–318`), the retry loop (`:225`) has no cancellation check (and could
not see one — the task is never cancelled), and `await task.value` on a
non-throwing task does not propagate waiter cancellation. Consequences: a
fast scroll through a large grid runs *every* passed cell to completion —
download plus decode — and with URLSession's per-host connection cap the
posters the viewer is currently looking at wait FIFO behind the backlog;
offline, `waitsForConnectivity` holds each orphan attempt up to 60 s. The
disk-warming half of this is arguably a feature; the starvation half is a
regression. Fix: refcount waiters in the coordinator and cancel the
underlying task when the last waiter leaves (`withTaskCancellationHandler`),
keeping coalescing intact. Verify on tvOS with the device pass below.

**4. `prune()` re-reads every metadata file on every store, on the same
actor serving reads.** `store` → `prune` (`:387`, `:416`) lists the whole
directory and JSON-decodes every sidecar; at capacity (~2 000 entries at a
~130 KB poster average) that is ~2 000 file reads per stored poster,
serialized on the actor that `entry(for:)` — the scroll path — also queues
on. First-fill of a big library does this once per poster while the
directory grows, and the init-time `Task { await diskCache.prune() }`
(`:155`) queues a full scan ahead of the first disk reads of every launch —
latency added to exactly the cold-start paint this PR exists to speed up.
Fix: scan once at init, keep a running byte total in the actor, prune only
when the total crosses the limit.

**5. `clear()`'s fire-and-forget disk wipe races in-flight stores**
(`:207–210`). A download that completes after sign-out/server-switch
re-writes its bytes after `removeAll()` and they persist for up to 30 days.
The key scheme makes strays unreadable (origin changed), so this is disk
residue and a privacy nit, not wrong artwork. Fix: a generation counter in
the actor — `removeAll` bumps it, `store` drops writes from old
generations.

**6. The integration layer is untested — the WO-04 pattern again.** The
disk actor itself is *well* pinned (see below), but no test touches the
wiring: flip `.terminalFailure`→`.transientFailure`, delete the
`removeAll()` from `clear()`, or delete the coordinator entirely, and the
suite stays green. Cheapest seam: extract the status classification into a
pure `static func classify(...)` and unit-test it (that also makes fix #1
pinnable); a `URLProtocol` stub for `refreshImage`'s store/remove wiring is
the fuller version but needs an injectable cache.

## Notes, no action required

- `Session.shared` is `@unchecked Sendable` with an unsynchronized
  `var origin` written on the main actor; this PR adds cooperative-pool
  reads (`localImage`, `sourceKey`). Pre-existing pattern — `image()`
  already read it off-main via `url(path)` — just now wider.
- A terminal failure keeps the stale image on screen for the current view's
  lifetime (only the caches are dropped). Looks deliberate; fine.
- Caches directory is the right home (purgeable, unbacked-up,
  tvOS-appropriate); SHA-256 hex filenames close traversal by construction;
  writes are atomic; prune cleans orphaned pairs in both directions;
  corrupt files self-heal (`entry` → decode fail → remove → refetch).
- Disabling `urlCache` removes the double-cache honestly. (The PR body's
  "restarting cleared all cached images" slightly overstates base — the
  default `URLCache` persisted a few MB — immaterial.)
- Two decode sizes coalescing on one source both write the same bytes to
  disk; harmless double store.
- Docs debt this PR grazes: apple README's test-coverage list doesn't
  mention the new cache tests, and `APPLE-CLIENT-PARITY.md` still says
  "build 37" while this makes 39 — both already WO-09 items.

## What holds up well

- **The three new tests are discriminating, not decorative.** I re-derived
  the LRU test's byte math by hand: three ~1 225-byte pairs against a 3 000
  limit force exactly one eviction, and the mid-test `entry()` read is what
  saves "first" — delete the modification-time touch at `:380` and the test
  fails. Expiry is pinned at the exact 30-day boundary, freshness at the
  7-day boundary, persistence across a re-opened instance, and the 64-char
  token shape. Date round-tripping through JSON is exact for the integral
  epoch chosen. This is the discriminating-power standard the retro asked
  for — applied to the actor, just not yet to the wiring above it.
- **The version gate actually passes this time.** Unlike #92, preflight is
  green, so CI's macos-15 job will really run the Swift suites.
- **Key design is right**: origin in the key isolates servers; absolute
  provider URLs deliberately shared across servers; size in the memory key
  only, so one stored original serves every decode size.

## Method and gates run

Cloud clone, `refs/pull/94/head` pinned at `3102069` in a detached
worktree; diff read in full; every `AuthImageCache` call site swept (no
stale references to the renamed API). Ran the full CI preflight locally on
the PR tree — all green:

```bash
python3 -m validation.ci_scope --event pull_request --base 74825a1
#   → apple=true · mobile_version=true · docs_only=false
PLURX_VALIDATION_MODE=changed-from PLURX_VALIDATION_BASE=74825a1 \
  python3 -m validation.mobile_versions        # EXIT=0 (38→39 accepted)
make history-check validation-lint operations-check   # all green
python3 -m unittest discover -s tests/validation      # 32 tests OK
```

Rust surfaces are untouched by this diff (clients/apple only), so the heavy
workspace gates were not re-run. Swift does not build in the Linux review
sandbox: the 116/114 simulator counts in the PR body and everything
device-observable below are GPT's to confirm.

## Device pass for GPT

```
Check out pjunod/plurx PR #94 (branch codex/persistent-artwork-cache,
head 3102069) and verify on real devices/sims:

1. cd clients/apple && xcodegen generate, then run the iOS and tvOS test
   schemes. Confirm both suites pass and report the exact counts.
2. Cold-cache first launch: browse a large library, force-quit, relaunch.
   Posters must paint immediately from disk, before any network.
3. Airplane mode after step 2's browse: relaunch offline. Cached posters
   must still paint; uncached cells show the placeholder, no hangs.
4. tvOS scroll regression check (PR review finding 3): on the largest
   library, fling-scroll several screens and stop. Time how long the
   now-visible row takes to fill vs. the shipped build 38. Report if it
   is visibly slower (downloads queue behind scrolled-past cells).
5. Sign out, relaunch: no artwork from the prior session may appear.
6. Report Settings → General → Storage delta for the app after a long
   browse (expect ≤ ~260 MB attributable to the artwork cache).
```
