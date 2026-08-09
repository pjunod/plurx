# WO-07 — Offline viewing follow-ups

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P1 (tasks 1-3), P2 (rest)**
Mixed Rust/Kotlin/Swift; client tasks need Android Studio/Xcode. Bump client build counters on release-path commits.

## Context

The offline implementation (#68/#69/#73) is the best-designed large feature of the week — the server core (content-addressed packages over the producer's claim/resume machinery, transactional quotas, typed errors, hashed leases, bounded metrics) verified against all twelve plan-review revisions. The weak half is client lifecycle plumbing and two coverage gaps on the feature's headline promises.

## Tasks

1. **Android: package release is lost for any download that paused or completed after a process restart.**
   `clients/android/.../OfflineDownloads.kt`: `prepare()`'s resume branch returns before `activeApis[id] = request.api` is set (~:218-221 vs :311), and `activeApis` is in-memory — after restart, `reconcile()` → `reflect(completed)` finds no API (~:409-415) and `completeOfflinePackage` is never called. Server keeps the `ready` row + pinned cache bytes + the user's row/byte quota for the full 7-day TTL; a 15 GiB profile quota can wedge on 507s.
   Fix: rebuild the API client from stored credentials inside `reflect` (or persist a "needs completion call" flag flushed on foreground).
   Acceptance: pause → resume → kill process mid-transfer → let it complete → server row deleted within one foreground session.

2. **Pin the headline arbitration ("live playback outranks preparation").**
   `ensure_offline`'s only caller is `OfflineManager::prepare`; no test invokes it, and no test exercises `offline_waiting` preempting the speculative producer. Reverting the wiring (spawning its own ffmpeg that ignores `live_is_waiting`) leaves every suite and the `offline-contract` validation gate green.
   Fix: (a) a paced-ffmpeg test running `ensure_offline` while an admission slot is held, asserting preemption + resume (parts ≥ 2); (b) a unit test that `produce()` stands down while `offline_waiting` is set. Model on `a_preempted_producer_resumes_without_losing_picture` (`transcode.rs:7333`), including its ffmpeg-capability skip guard.
   Also: confirm CI runners actually have ffmpeg ≥5.1 — that existing preemption test self-skips without it, which would silently remove the only arbitration coverage. Make the skip loud (print + a nightly assert that it ran).

3. **Apple: catalog read-modify-write across actor awaits can regress a completed download.**
   `OfflineDownloadManager.swift`: delegate callbacks spawn unordered `Task`s; a late progress/`didLoad` task (~:555-564, sets `.downloading`) can run after completion set `.downloaded` (~:629-656). Item shows downloading forever; next launch marks it paused; Resume's poll 404s (package already completed server-side) → user must delete and re-download.
   Fix: mutate inside the actor via a single `catalog.update(id) { }` seam, and drop progress writes once the task no longer exists.
   Acceptance: unit-test the ordering with injected task completion order; device-verify a normal download still lands `.downloaded`.

4. **Kill switch should also stop delivery.** `offline_enabled=false` cancels producers but media routes never check it (`http/offline.rs:714-773`) — devices keep pulling ready packages for up to 7 days. Gate `authorized_package` on the setting (or document the choice in OPERATIONS.md). Acceptance: disable → leased segment fetch → 503.

5. **Admin per-item cancel.** `DELETE /offline/packages/{id}` is owner-scoped only; the activity page shows a 6-hour prepare with no stop control — the global switch cancels everyone's work. Add admin-scoped delete + a stop button on the activity page (matches the existing `stop_session` pattern and the owner's attribution rule). Acceptance: admin cancels one package; others unaffected.

6. **Queue fairness.** `claim_next_offline_package` is strict FIFO by `created_at` — one user queueing 50 films serializes everyone else behind days of encode. Round-robin by user on claim. Acceptance: two users' queues interleave in a store test.

7. **P2, coordinate with WO-06/WO-09:** Apple offline playback has no stall watchdog (`startOffline` never calls `startPlaybackRecoveryMonitor` — decide: enable the silent-freeze leg offline); offline items carry `markers: []` on Apple (`DetailView.swift:1617-1625` never passes them) so downloaded episodes have no Skip Intro/Credits — plumb markers into the queued payload.

## Don't

- Don't redesign the lease (client-generated 256-bit token, SHA-256 stored, 7-day rolling expiry, renew-with-same-token) — reviewed sound. The rotation-forces-re-download residual is accepted.
- Don't add a FK from packages to files — the deliberate non-FK + migration test (`v14_offline_packages_do_not_cascade_with_rescanned_files`) is the accepted design; the residual `transcode_cache_recipes.file_id` cascade (stranding a ready package's bytes after a rescan, honest 410 to clients) is known and tolerable.
- Don't re-review the quota/idempotency transaction shape — `quota_check_and_insert_share_the_transaction` pins it. (But see WO-02: #90's branch touches `same_request` — that fix lands there.)
