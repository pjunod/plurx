# Integration plan — plurx's side of the monarr pipeline

**Status:** ready to build · **Written:** 2026-07-26 ·
**Master plan:** `monarr/docs/plan-integration.md` (contract §3, phasing §7)

This document is self-contained: everything plurx must build, with the
exact contract slice it implements, so a session opened in this repo
alone can do the work. The master plan in the monarr repo holds the
cross-app rationale and phasing; if this file and the master disagree,
the master wins and this copy is the bug.

The shape of the whole thing: when monarr finishes importing media into
a library folder, it will call plurx — "scan exactly this path, here
are the TMDB/IMDb ids, tell me what you made of it." plurx's job is to
make that call possible (today nothing accepts a path), safe (today the
only credential that can trigger scans is a full admin token that can
also read the TMDB/Trakt secrets out of `/settings`), and honest (the
caller gets the scan report back, and the whole thing is visible in
plurx's own activity/logs). Plus one adjacent gap this plan closes:
plurx currently has **no scheduled scans at all** — the targeted scan
becomes the fast path, and a new periodic reconcile becomes the safety
net beneath it.

Work milestone by milestone, in order. Each ends with an acceptance
check. If a step seems to require writing to media storage or widening
what an API key can do, stop and flag it instead.

---

## 1. Contract slice (copied from master §3.5 on 2026-07-26)

Re-verify every identifier against the named file before building.

**API keys** — new concept beside user tokens:

```
POST   /api/v1/keys        (admin)  {name, scopes:["scan:trigger",…]}
                           → {id, name, scopes, key:"plx_<32 hex>"}
GET    /api/v1/keys        (admin)  → list, no secrets, last_used_at
DELETE /api/v1/keys/{id}   (admin)
```

Presented as `Authorization: Bearer plx_…`; the `plx_` prefix routes
verification to key lookup (SHA-256 at rest, like user tokens) instead
of user-token lookup. Scopes now: `scan:trigger`, `status:read`. A key
has exactly its scopes: it cannot read `/settings`, manage users, or
stream media.

**Targeted scan:**

```
POST /api/v1/scan                    (scope scan:trigger)
{
  "path": "/media/movies/Heat (1995)",         // absolute; dir or file
  "ids": { "tmdb": 949, "imdb": "tt0113277" }, // optional
  "hint": "movie",                             // movie|episode|season, opt
  "series": { "tmdb": 1396 },                  // episodes: the SHOW's id
  "correlation_id": "t-42-a3f9c1",             // opt, echoed everywhere
  "source": "monarr"
}
```

Responses (normative):

- Path not under any library root →
  `422 {"error":"path is not under any library root","roots":[…]}` —
  self-explaining so a path-mapping mistake in monarr diagnoses itself.
- Scanner idle for that library → scan the subtree **synchronously**,
  return `200 {"status":"scanned","library_id":3,"report":{added,
  updated,unchanged,removed_files,skipped,errors,problems[]},
  "items":[{"item_id":1201,"file_id":88,"path":"…"}],
  "correlation_id":"…"}`.
- Scanner busy → coalesce and return
  `202 {"status":"queued","request_id":"sr-…"}`;
  `GET /api/v1/scan/requests/{id}` (scope `status:read`) →
  `{status: queued|running|done|failed, report?, items?}`.
- A targeted scan **never reconciles/prunes** anything outside what it
  found under `path`.
- When `ids` are present: apply via `MetadataPatch{tmdb_id, imdb_id}` →
  `apply_metadata`, mark matched so `items_needing_metadata` skips the
  item, enrich by id (no `find_movie(title, year)` fuzzy pass). For
  episodes, `series.tmdb` belongs to the show item; season/episode
  numbers still come from the filename parse.

---

## 2. Milestones

### P1 — scoped API keys

The gap: auth is user tokens only (`crates/plurxd/src/http/auth.rs`,
extractors in `http/extract.rs`); the only tier is `is_admin`, and
admin can read secrets out of `GET /api/v1/settings`. monarr must hold
a credential that can trigger scans and *nothing else*.

- Storage: new table (schema bump — CHANGELOG says v6 today, so this
  is v7): `api_keys {id, name, key_hash, scopes (JSON array), 
  created_at, last_used_at, disabled}`. Hash = SHA-256 of the secret,
  same discipline as `auth::hash_token`.
- Secret format `plx_` + 32 hex (16 random bytes); shown once in the
  create response, never retrievable again.
- New extractor beside `AuthUser`/`AdminUser` — e.g.
  `Scoped<const S: &'static str>` or a runtime
  `ScopedKey(scope)` guard — that accepts `Authorization: Bearer
  plx_…` only, checks the scope, bumps `last_used_at`. User tokens do
  **not** pass key-scoped routes, and keys do not pass user routes:
  two credential kinds, two doors.
- Routes per §1, admin-gated, plus a Keys section in the settings UI
  (`crates/plurxd/src/web/index.html` — single-file UI) with create /
  copy-once / revoke.
- `docs/SECURITY.md` gains a keys section in the same commit (it
  currently documents the three token presentations; keys are a fourth
  credential and its scope story must be written where the auth story
  lives).

**Accept:** http tests: create key → call `/api/v1/scan` with it (200
path, after P3); same key on `GET /api/v1/settings` → 403; disabled
key → 401; user token on `/api/v1/scan` → 401/403 (key-only route);
`GET /api/v1/keys` never returns the secret.

### P2 — targeted scan core

The gap: `scan::scan_library_with_progress`
(`crates/plurx-core/src/scan/mod.rs`) takes a `&Library` and walks
every root; there is no path-shaped entry point, and full-library
reconcile would delete rows it didn't see.

- New core fn, e.g.
  `scan_path(store, &Library, sub: &Path, ids: Option<&IdHints>)
  -> ScanReport` + placed-item list:
  - Validate: canonicalize `sub`, require component-wise prefix match
    against one of `library.paths` (reject traversal; a symlink that
    escapes the root fails validation — same rule as full scan's
    `follow_links` is *within* a root).
  - Walk only `sub` (or the single file), reuse the existing pipeline
    pieces: `is_video()` filter → `place_item()` (`parse::parse_movie`
    / `parse_episode` / anime variant per `library.anime`) →
    `probe::probe` → `store.upsert_file`. Size+mtime unchanged-skip
    stays.
  - **No reconcile step, ever** — mirror the existing
    `walk_errors > 0` precedent: pruning against a partial view would
    delete the rest of the library.
  - Id application per §1: after placement, `apply_metadata` with the
    ids, mark matched, then enrich that item by id (P4).
- Return the same `ScanReport` shape the full scan produces, plus
  `(item_id, file_id, path)` per placed file — the caller is owed an
  answer, not a shrug.

**Accept:** unit tests: scanning `roots[0]/Movie A` upserts Movie A and
leaves an existing Movie B row untouched (row-count property =
no-prune proof); a path outside all roots errors; a symlink escaping
the root errors; unchanged file short-circuits before probe.

### P3 — the endpoint, the queue, the visibility

The gap: `JobManager::trigger` (`crates/plurxd/src/state.rs`) *drops* a
trigger while a scan runs — a season import firing N notifications in
seconds would lose N−1 of them. And nothing today records that an
external system asked for anything.

- `POST /api/v1/scan` + `GET /api/v1/scan/requests/{id}` per §1,
  key-scoped.
- Library resolution from the path (plurx-side, so monarr never needs
  to know plurx's library ids).
- `JobManager` gains a per-library pending set: targeted requests
  arriving while that library scans are coalesced (dedup by path) and
  drained when the running job finishes; a full-library trigger
  supersedes pending targeted ones (it covers them).
- Request records: bounded ring (256) of
  `{request_id, at, source, correlation_id, path, status, report?,
  items?}` — in memory is acceptable (documented as such): it is a
  debugging surface, not an audit log.
- Visibility, all four of it: (a) `tracing` under a dedicated target
  `plurxd::integrate` with `correlation_id` as a structured field —
  greppable in `GET /api/v1/system/logs`; (b) the request ring exposed
  in `GET /api/v1/activity/detail` next to the existing `scans` block;
  (c) a `last notification` line (source · path · result · when) in
  `GET /api/v1/system` for the monarr Connections panel to read;
  (d) metrics: `plurx_scan_total{trigger="api|create|update|schedule|
  notify"}`, `plurx_notify_received_total{source,result}`,
  `plurx_scan_duration_seconds`. `/metrics` currently has no scan
  counters at all.
- Modest rate limit on `/api/v1/scan` (token bucket like the existing
  `client-log` one) — a runaway caller should get 429s, not a melted
  NAS.

**Accept:** http tests: idle → 200 with report and items; busy → 202,
then request completes and `GET /scan/requests/{id}` shows `done` with
the report; three requests for the same path while busy → one pending
entry; metrics counters move; the system-info line updates.

### P4 — enrich by id

The gap: enrichment matches by `find_movie(&title, year)` /
`find_show(...)` fuzzy search (`crates/plurx-core/src/metadata/mod.rs`)
even when the caller *knows* the id. Wrong matches also poison Trakt
sync, which matches by TMDB id.

- When an item arrives with `tmdb_id` already set (P2's id
  application), fetch details directly by id (movie detail / show
  detail — the tmdb client already fetches by id for seasons) and skip
  the search entirely.
- `items_needing_metadata` must not re-fuzzy an id-matched item;
  `force` (the `/refresh` path) refreshes *by the stored id*, not by
  title.

**Accept:** unit test with a deliberately misleading title
(`"Heat (1995) Directors Cut Remux"` with `tmdb_id` for Heat) enriches
to the id's canonical title, and the fuzzy search fn is never called
(assert via a counting fake client).

### P5 — scheduled reconcile scan

The gap: plurx has **no scheduled scans** — today the library drifts
until someone presses the button. With targeted scans as the fast
path, a slow full reconcile underneath catches everything else
(manual file moves, deletes, anything not announced by monarr).

- New DB setting `scan.interval_hours` (store `keys::` constant +
  `SettingsDto`/`UpdateSettings`; default `12`, `0` = off). **Not** a
  TOML key — the config structs are `deny_unknown_fields` and runtime
  knobs live in the DB by convention.
- A tokio interval loop started in `run()` beside the trakt loops:
  every interval, `trigger_scan` per library (the existing full-scan
  path, which does reconcile/prune — that is its job here).
- Skip a library whose scan is already running; log the skip.
- Surface next-run time in `GET /api/v1/scan/status` or system info so
  the schedule is visible, not folklore.

**Accept:** with interval set to a test-scale value, both libraries
scan on schedule; setting `0` stops the loop; next-run visible via the
API.

### P6 — integration visibility (and docs)

> **Note added on building it.** This file called P6 "docs"; the master
> plan's §6 table calls it *integration visibility* — the `plurxd::integrate`
> target, a last-notification record in `/api/v1/system`, and the
> `plurx_scan_total{trigger}` / `plurx_notify_received_total` metrics. The
> master wins on conflict, so both were built: the docs below, and the
> counters.

**Counters, and why these two.** A plurx that scanned 400 times today tells
you nothing. `plurx_scan_total{trigger="scheduled"} 398` beside
`{trigger="targeted"} 2` tells you the integration has quietly stopped and
the slow sweep is carrying everything — which looks entirely fine from the
library, just slower, for as long as nobody looks.
`plurx_notify_received_total` counts every inbound scan request **before**
the path is resolved, so a request rejected for a container path-mapping
mistake still proves the caller reached plurx with a working key. That is
what separates "fix monarr's path mapping" from "check monarr's URL and
key", and `scan_requests` alone cannot: a rejected request never gets as far
as having one.

The same facts are on `GET /api/v1/system` under `integration`, for anyone
without Prometheus.

#### Docs

Same-commit rule: `docs/SECURITY.md` (keys — done in P1),
`docs/FEATURES.md` (integrations section: what arrives from monarr and
what plurx does with it), `docs/OPERATIONS.md` (pairing runbook: create
a `scan:trigger` key → paste into monarr's plurx notifier → import
something → verify on monarr's Connections panel and in plurx activity),
`docs/ROADMAP.md` (Phase-1's deferred watcher note gets a pointer:
inotify remains future work; scheduled + targeted scans are what
shipped instead), `docs/CHEATSHEET.md` (the two curl lines: create a
key, trigger a scan).

**Accept:** `grep -ri "scan:trigger" docs/` hits SECURITY, OPERATIONS,
CHEATSHEET.

---

## 3. Later phases — sketched only, re-scope before building

Committed in principle (master plan §11), not designed in detail here:

- **P7 watched → monarr:** DB settings `monarr.url` / `monarr.api_key`
  / `monarr.watched_sync` (default off); on scrobble / the existing 95%
  auto-watch crossing, queue
  `POST {monarr}/api/v1/webhooks/plurx {event:"watched", kind, tmdb,
  imdb, season, episode, watched_at}` — aggregate signal, no usernames;
  retry with backoff; visible in the same `plurxd::integrate` log
  stream. This is plurx's first outbound webhook — build the tiny
  delivery queue then, not now.
- **P8 coming-soon rail:** plurxd proxies `GET /api/v1/coming-soon` →
  monarr `GET /api/v1/calendar` using a monarr API key from DB
  settings (server-side only; the key never reaches a browser), cached
  15 minutes, rendered as a home rail.

## 4. Guardrails (stop and flag rather than violate)

1. **plurx never writes to media storage** — existing invariant
   (ARCHITECTURE §8). The scan request names a path to *read*.
2. **Keys are least-privilege or they are wrong** — if the monarr key
   can read `/api/v1/settings` or list users, P1 has failed its point.
3. **Targeted scans never prune.** No exceptions, including "the
   folder is empty now" — deletion is the reconcile scan's job.
4. **No new TOML keys** — `deny_unknown_fields` is a feature; runtime
   knobs go in the DB settings store.
5. **The Plex façade is untouched** — no scan verbs there; it stays
   read+watch-state only.
6. **Cluster forward-compat:** ARCHITECTURE §2.2 plans scans as a
   leader-scheduled singleton. Keep all trigger paths (endpoint,
   schedule, pending drain) going through `JobManager` so that seam
   stays single.
