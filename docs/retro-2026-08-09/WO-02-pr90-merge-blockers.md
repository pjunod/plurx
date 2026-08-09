# WO-02 — PR #90 (clustering M1d): last fixes before merge

**Repo:** `~/code/plurx` · **Branch:** `codex/clustering-m1d-remaining-store` @ `2109946` (PR #90) · **Priority: P0 — do this before merging #90**

## Context

The #90 review raised 9 blockers + 13 should-fixes; the fix commits (`e926ee8`, `2109946`) were re-reviewed and **essentially everything is genuinely fixed** — the coalescer now re-reads durable state per beat and flushes via full-row CAS (`put_progress_if_current`), a shutdown drain landed inside the Docker stop budget, the outbox got claim leases, Trakt tokens got CAS with loser-adoption, the byte guard is node-scoped, the gate varies identity with refused-transition post-conditions, and the store contract suite runs against a live 3-voter cluster in CI. Deferred items (leader gating, token encryption at rest, removed-node cleanup, post-coalescer growth remeasure) are written down as M2 activation gates — acceptable.

One regression introduced by the fix commit blocks merge; two minor items ride along.

## Tasks

1. **BLOCKER — drop `expires_at` from `same_request`.**
   The handler derives expiry fresh per attempt: `let expires_at = now_unix().saturating_add(PACKAGE_TTL_SECS);` (`crates/plurxd/src/http/offline.rs:384` at the PR head). `e926ee8` added `existing.expires_at == requested.expires_at` to `same_request` on **both** backends (`store/sqlite/offline.rs` ~:79-81; `hiqlite_durable.rs` ~:1068-1070). Net effect on the shipping SQLite path the moment this merges: a client POSTs a download, the response is lost, it retries with the same `request_id` one second later → different `expires_at` → `RequestConflict` → HTTP 409 "already used with different download options" instead of the idempotent `Existing`. `expires_at` is server-clock-derived, not a client-chosen option — it doesn't belong in request identity.
   Fix: remove it from `same_request` on both backends; optionally refresh expiry when returning `Existing`.
   Acceptance: flip the existing contract test that currently PINS the wrong behavior (it does `changed_expiry.expires_at += 1` → expects `RequestConflict`) to expect `Existing`, and add the retry-shaped test: same struct, `expires_at + 1`, expect `Existing`.

2. **Minor — `drain()` aborts on first store error** (`crates/plurxd/src/progress.rs:361-364`): one failing entry abandons the rest of the pending flushes inside the 3 s budget. Continue per-entry, return the first error afterward. Acceptance: drain with two pending entries and one injected failure still flushes the other.

3. **Minor — pre-lock durable read races the flush worker** (`progress.rs:118-121`): a flush committing in that window causes a spurious divergence reset and one extra durable commit. No correctness harm. Either take the read under the entry lock or add a comment accepting the extra commit; a paused-time test pinning the commit count is nice-to-have.

4. **Hygiene note for the merge message:** `e926ee8` bundles unrelated Apple-client/transcode buffer tuning into the "address review" commit. Don't rewrite history for it, but name it in the PR description so the CHANGELOG entry doesn't attribute those changes to clustering.

## After merge

- Confirm CI's cluster job fits its new 20-minute timeout on a real runner (the 3-voter contract suite is new load).
- The coalescer ships to every SQLite install at merge — watch `plurx` logs for `flush` warnings in the first days (MAX_FLUSH_ATTEMPTS=8 with backoff is the new bound).
- Check whether the Apple/Android clients actually reuse `request_id` on POST retry (that determines task 1's real-world blast radius; repo docs imply yes).

## Don't

- Don't re-litigate the durability model: the "ack ≠ durable for progress beats, documented as the explicit exception" resolution in ARCHITECTURE.md §2.2 was reviewed and accepted.
- Don't re-audit the M1d SQL port (schema parity, digests, node-scoping, lease/claim atomicity under contention) — verified clean.
- The heartbeat-validation change in `2109946` (poll-until-visible ≤5 s replacing an instant assertion) was reviewed: it is a legitimate accommodation of local (non-consistent) cache reads, not a weakened gate.
