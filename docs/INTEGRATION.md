# Integration — plurx's seams with monarr, and how to prove they work

Companion to [INTEGRATION-PLAN.md](INTEGRATION-PLAN.md) (the master plan and
why each seam exists) and [OPERATIONS.md](OPERATIONS.md) (running it day to
day) — this is *what each seam does, where you look at it, and the exact
command that proves it*. The trust model behind the scoped-key wall is in
[SECURITY.md](SECURITY.md).

plurx is the **last stage** of a three-application pipeline. It plays what the
other two put on disk. Its only integration partner is monarr: plurx and nzbd
have no seam at all, by design — every path between them runs through monarr,
after the import, when the files are actually in place.

## The pipeline

```
   ┌──────────────┐   grab   ┌──────────────┐
   │    monarr    │ ───────▶ │     nzbd     │      nzbd and plurx never
   │  (decides)   │ ◀─────── │ (downloads)  │      speak. There is no seam
   └──┬────────▲──┘   SSE    └──────────────┘      between them, on purpose.
      │        │
      │        │  POST /api/v1/webhooks/plurx   (watch state, opt-in)
      │        │  GET  /api/v1/calendar         (the Coming soon rail)
      │        │  GET  /api/v1/system/status    (Test connection)
      │        │
      │ POST /api/v1/scan   (scoped key, correlation_id = monarr's transfer id)
      ▼        │
   ┌───────────┴──┐
   │    plurx     │
   │  (plays it)  │
   └──────────────┘
```

Default ports: nzbd 6789 · monarr 7676 · **plurx 32600**.

## Wiring the two together in Docker

Do this first. Most "cannot reach monarr" is a container that cannot resolve a
name, not a wrong key or a wrong port.

**One shared user-defined network, and address everything by container name.**
That is the recommended setup, not merely one that works.

```bash
docker network create media          # once, on the host
```

Then in **each** compose file — plurx's, monarr's, nzbd's — attach the service
and declare the network as external:

```yaml
services:
  plurx:
    container_name: plurx            # this name IS the hostname
    # ...the rest of your service...
    networks:
      - media

networks:
  media:
    external: true                   # created above; compose must not own it
```

`docker compose up -d` in each directory, then set the monarr URL to
`http://monarr:7676`.

Three things that trip people up, each with its reason:

**The hostname is the `container_name`, or the service key if you did not set
one.** Not the image, and not the directory.

**Published ports are irrelevant on this path.** `ports: - "32600:32600"` maps
host→container; container→container traffic goes straight to the *internal*
port and works even with no published port at all. Keep the published ones for
your own browser, and stop reasoning about them when debugging a seam.

**`host.docker.internal` is a per-container setting, and it is the wrong tool
here.** It only exists in a container whose compose declares
`extra_hosts: ["host.docker.internal:host-gateway"]`. plurx is the process
making this call, so that line has to be on *plurx's* service — putting it on
monarr's does nothing, which is exactly the trap: monarr→nzbd works, plurx→
monarr does not, and the two look symmetric from the settings screen. It also
routes container→host→container for traffic that never needed to leave the
bridge. A valid fallback when you cannot edit every compose file; not the good
answer.

**How to tell which one you are hitting.** Test connection reports the root
cause, not the wrapper: `dns error: failed to lookup address information`
means the name is not resolvable from this container — wrong network, or a
missing `extra_hosts`. `Connection refused` means the name resolved and
nothing was listening on that port.

## The seams plurx has

| # | Seam | Direction | Transport | Who starts it |
|---|---|---|---|---|
| 1 | Targeted scan | **inbound** | `POST /api/v1/scan` | monarr, on import |
| 2 | Scoped API keys | the wall around §1 | `/api/v1/keys` | you, once |
| 3 | Coming soon rail | outbound | `GET {monarr}/api/v1/calendar` | plurx, every 15 min |
| 4 | Test connection | outbound | `GET {monarr}/api/v1/system/status` | you |
| 5 | Watched outbox | outbound, durable | `POST {monarr}/api/v1/webhooks/plurx` | plurx, on watch |

---

## 1. Targeted scan — "this exact folder changed"

**What it does.** When monarr finishes an import it names the directory,
instead of leaving plurx to find it on the next scheduled sweep. One request
per directory; a season pack that lands ten episodes in one folder is one
thing that changed.

**Wire.** `POST /api/v1/scan`, header `X-Api-Key: plx_…` with scope
`scan:trigger`:

```json
// an episode: the SHOW's id, under `series`
{"path":"/media/tv/Show/Season 02","hint":"episode","series":{"tmdb":1399},
 "correlation_id":"t-42-a3f9c1","source":"monarr"}

// a movie: its own ids, under `ids`
{"path":"/media/movies/Some Film (1999)","hint":"movie",
 "ids":{"tmdb":603,"imdb":"tt0133093"},
 "correlation_id":"t-43-b1e207","source":"monarr"}
```

`path` is absolute and must resolve **under a configured library root** — a
request for anything else is rejected, which is also how monarr's Test button
works (§4 of monarr's own integration doc: it deliberately probes an
unresolvable path and counts the rejection as a pass).

`hint` is advisory. The library's own kind decides how a file is parsed; the
hint only picks which item an id applies to. For an episode, `series.tmdb` is
the **show's** id — an episode's own id does not identify the series.

`correlation_id` is monarr's transfer id (`t-<downloadID>-<hex>`), echoed back
in the response and written to the log, so one grep across three applications
reconstructs a single transfer.

Two success shapes, and the difference matters:

| Code | Body | Means |
|---|---|---|
| `200` | `{"status":"scanned","library_id":…,"request_id":…,"report":…,"items":…,"correlation_id":…}` | scanned inline; the result is in the body |
| `202` | `{"status":"queued","request_id":…,"correlation_id":…}` | plurx was busy — queued, not dropped |

Poll `GET /api/v1/scan/requests/{id}` (scope `status:read`) for
`queued | running | done | failed`. Silently losing a request would leave a
season half-indexed, so a busy plurx queues rather than refusing.

The out-of-roots rejection is a `422` with
`{"error":"path is not under any library root","path":…,"roots":[…]}` — the
`roots` list is deliberately included, because "wrong path" is only actionable
if you can see what the right ones would have been.

**Where you see it.** Settings → **Libraries** shows the scan itself running:
`scanning… N / M files` → `fetching metadata…` → `X added, Y updated, Z
unchanged`. It does **not** attribute the scan to its trigger — see the honest
gaps section below.

The one place an inbound request is visible as an inbound request is
Settings → **System → Logs** at `Info+`, target `plurxd::integrate`.

**How to verify.**

```bash
PLURX=http://127.0.0.1:32600
PLXKEY=plx_…                        # from §2

curl -sS -X POST "$PLURX/api/v1/scan" \
  -H "X-Api-Key: $PLXKEY" -H "Content-Type: application/json" \
  -d '{"path":"/media/movies/Some Film (1999)","source":"manual-test",
       "correlation_id":"t-0-verify"}'
```

**How to read it.** A rejection naming *"under any library root"* proves the
key, the scope and the route all work — it is the wrong path and nothing else.
A 401 means the key is wrong or revoked; a 403 means the key is real but
lacks `scan:trigger`. `"status":"accepted"` with a request id means plurx took
it; whether the file was *usable* is the scan's business, not the seam's.

---

## 2. Scoped API keys — why monarr never holds an admin token

**What it does.** A key is what another application holds *instead of* a login
token. The difference is not cosmetic: a token **is** a user, so an admin
token handed to a neighbouring app also hands over every secret in
`GET /api/v1/settings`. A key carries a scope list and cannot widen itself.

**Scopes.** Exactly two exist: `scan:trigger` (ask for a scan — the whole
point of the monarr integration) and `status:read` (read the progress of a
scan this key asked for). Creating a key with an unknown scope is rejected
rather than stored, because a typo'd scope produces a key that looks correct
in a list and authorizes nothing.

**Where you mint one.** There is **no UI for keys yet** — it is a curl.

```bash
# admin session cookie required
curl -sS -X POST "$PLURX/api/v1/keys" \
  -H "Content-Type: application/json" -b <admin session> \
  -d '{"name":"monarr","scopes":["scan:trigger"]}'
```

The response carries `key_secret` — `plx_…` — **exactly once**. It is not
retrievable afterwards, which is the correct cost of never storing it. Lose it
and you issue a new key.

```bash
curl -sS "$PLURX/api/v1/keys"    -b <admin session>   # list; shows last_used_at
curl -sS -X DELETE "$PLURX/api/v1/keys/<id>" -b <admin session>   # revoke
```

**How to read it.** `last_used_at` on the listing is the cheapest proof that
monarr is really using the key you think it is. A key that has never been used
and a monarr that reports successful deliveries means monarr is holding a
*different* key.

Note what the listing does **not** return: `key_hash`. Listing keys is a
routine, frequently-open screen; the stored hash has no business on it, and
leaving it out means it cannot leak from there.

---

## 3. Coming soon rail — plurx reading monarr's calendar

**What it does.** The home screen shows episodes airing and films due in the
next four weeks, read from monarr's calendar. plurx proxies the call
server-side so the monarr key never reaches a browser.

**Wire.** `GET {monarr_url}/api/v1/calendar?start=&end=` with
`X-Api-Key: <monarr key>` and UA `plurx/<version>`. Cache TTL **15 minutes**,
horizon **28 days**.

**Where you configure it.** Settings → **Metadata** → the **monarr** card:
`monarr URL` (placeholder `http://monarr:7676`), `monarr API key` (from
monarr's Settings → Security → Reveal), and the `Send watch state to monarr`
checkbox (§5).

**The URL is completed on save.** A bare host — `monarr`,
`host.docker.internal` — becomes `http://<host>:7676`, monarr's own default
port, and the field shows the completed form back so the change is visible
rather than magic. A scheme you supply is respected in full, port and all:
guessing `:7676` onto an `https://` URL behind a reverse proxy would break a
setup that was already correct.

See [Wiring the two together in Docker](#wiring-the-two-together-in-docker)
below — in a container setup, the URL is only half the problem.

**Where you see it.** Home → the **Coming soon** rail.

**How to verify.**

```bash
curl -sS -b <session> "$PLURX/api/v1/coming-soon" | python3 -m json.tool
```

**How to read it.** This is the seam with the sharpest edge in the whole
integration: **the rail fails silently.** An unreachable monarr, a rejected
key, or a genuinely empty calendar all produce the same thing — no rail at
all. There is no error state on the home screen, on purpose (an error banner
on the home screen for an optional rail is worse than the missing rail), but
it means *absence of the rail is not evidence of an empty calendar*.

Worse, **Test connection (§4) probes `/api/v1/system/status`, not the
calendar.** A key valid for status but wrong for the calendar reads
`✓ connected` beside a permanently empty rail. When the rail is missing and
Test says connected, check the Logs card for target `plurxd::integrate` and
the message `coming-soon fetch failed`, or run the curl above.

---

## 4. Test connection — an active probe, deliberately

**What it does.** The **Test connection** button on the monarr card actively
calls monarr rather than repeating your config back at you. It distinguishes
three failures that look identical from the settings screen: cannot reach it,
reached it and the key was rejected, reached it and it works.

**Wire.** `GET /api/v1/monarr/status` (admin) → server-side
`GET {monarr_url}/api/v1/system/status` with `X-Api-Key`, UA
`plurx/<version>`, 10 s timeout.

**Where you see it.** Settings → **Metadata → monarr**. Literal results:

| Shown | Means |
|---|---|
| `checking…` | in flight |
| `✗ not configured` | no URL or no key saved |
| `✓ connected` / `✓ connected · monarr <version>` | reachable and authorized |
| `✗ monarr rejected the API key` | 401/403 — wrong key |
| `✗ monarr returned <status>` | reached something, but not monarr |
| `✗ cannot reach monarr at <url>: <err>` | DNS, port, or firewall |

`<err>` is the **root cause**, not reqwest's outer wrapper — the difference
between `dns error: failed to lookup address information` and
`Connection refused` is the entire diagnosis, and the outer message throws it
away. DNS means the other container is not resolvable from this one (a missing
`extra_hosts`, or a network they do not share); refused means the name
resolved and nothing was listening on that port.

Saving the card runs the test automatically. Beneath it, the queue line —
`Watch notifications — N sent, N waiting, N failed` — appears when any counter
is nonzero (§5).

Note the card's `Paired.` line, above the fields, is an **echo of stored
config**: it means a URL and a key are saved, nothing more. The status span is
the probe.

**How to verify.**

```bash
curl -sS -b <admin session> "$PLURX/api/v1/monarr/status" | python3 -m json.tool
```

**How to read it.** `✓ connected · monarr 0.9.0` proves URL + key + reachable,
and nothing about the calendar (§3) or the webhook (§5). It is the floor, not
the ceiling.

---

## 5. Watched outbox — plurx's only outbound push, off until you say so

**What it does.** When someone finishes something, plurx tells monarr — which
records it, displays it, and prefers upgrades for actively watched shows.
Nothing is deleted or unmonitored as a result; that path does not exist in
either application.

**Opt-in.** The `Send watch state to monarr` checkbox is off by default,
because the payload names the user who watched it. Per-user with usernames is
the deliberate choice — an aggregate "someone watched this" is useless for the
one thing monarr does with it — and that choice is precisely why it is opt-in
rather than on.

**Wire.** `POST {monarr_url}/api/v1/webhooks/plurx`, header `X-Api-Key`:

```json
{"event":"watched","kind":"episode","tmdb":1399,"season":2,"episode":5,
 "watched_at":1753500000,"user":"paul"}
```

`kind` is `movie` or `episode`. For an episode, `tmdb` is the **show's** id —
plurx walks episode → show to find it, because an episode's own id is not what
identifies the series it belongs to.

**Durability.** Deliveries go through a SQLite outbox with the retry schedule
as a column: backoff `5s → 30s → 2m`, batch of 20. Restart plurx mid-retry and
the retry still happens. A queue that lives in memory loses everything on the
one event you most want it to survive — a restart.

**Where you see it.** Settings → Metadata → monarr card, the queue line
under **Test connection**: `Watch notifications — 41 sent, 2 waiting, 0
failed`. And on the monarr side, System → **Connections** lists `plurx` as
`calls Monarr` / `calling`.

**How to verify.**

```bash
curl -sS -b <admin session> "$PLURX/api/v1/monarr/status" \
  | python3 -m json.tool | grep watched_
```

Then play something to the end and watch `watched_sent` move.

**How to read it.** `waiting` climbing is monarr being down; it drains itself.
`failed` is terminal — the delivery was classified as permanently unfixable (a
rejected key, a 4xx that will never become a 2xx) rather than retried every
two minutes forever. `sent` moving while monarr shows nothing means the ids
did not match: monarr matches on TMDB/IMDb id and never on title, and answers
`{"matched": false}` with a **200** for anything it does not manage. That is
not an error, and plurx correctly does not retry it.

---

## Metrics

`GET /metrics` (unauthenticated, counts only). Integration-relevant names:

| Metric | Type | Reads |
|---|---|---|
| `plurx_scan_total{trigger="…"}` | counter | scans started, by what asked for one |
| `plurx_notify_received_total` | counter | scan requests received from other applications |
| `plurx_watched_outbox{status="pending\|ok\|failed"}` | gauge | the §5 queue |

`plurx_scan_total{trigger="targeted"}` climbing is the clearest machine-
readable proof that §1 is live. `plurx_notify_received_total` flat while
monarr's delivery log shows `ok` means the requests are reaching something
that is not this plurx.

---

## Verify the whole chain in five minutes

```bash
PLURX=http://127.0.0.1:32600;  MONARR=http://127.0.0.1:7676
PLXKEY=plx_…                   # §2
MKEY=<monarr Settings → Security → Reveal>
```

1. **plurx can reach monarr** — Settings → Metadata → monarr →
   **Test connection** → `✓ connected · monarr <version>`.
2. **The calendar half works too** (Test does not cover it) —
   `curl -sS -b <session> "$PLURX/api/v1/coming-soon"` returns entries, and the
   home screen shows a **Coming soon** rail.
3. **monarr can reach plurx** — on monarr: Settings → Notifications → the
   plurx notifier's Test → a rejection naming *"under any library root"* is a
   pass.
4. **plurx is visible from monarr** — monarr: System → Connections lists
   `plurx` as `calls Monarr` / `calling`.
5. **End to end** — grab something in monarr. When it imports, monarr's
   delivery log reads `ok` / `scanned → plurx item …`, `plurx_scan_total`
   increments, and the file appears in plurx without you touching a scan
   button.
6. **Watch state, if enabled** — play something to the end;
   `watched_sent` increments on the monarr card and monarr records the watch.

---

## What plurx deliberately does not do

- **Never writes to media storage.** Libraries are mounted read-only. plurx
  never renames, moves, or deletes a file, and no integration changes that.
- **Never talks to nzbd.** There is no seam. plurx learns about new files from
  monarr, after the import, when they are actually in place — telling it
  earlier would only announce files that are not there yet.
- **Never accepts a scan from a login token.** `POST /api/v1/scan` is
  key-scoped only. The wall runs both ways: an application cannot use a user's
  token, and a key cannot become a user.
- **Never returns a key secret twice**, and never returns `key_hash` at all.
- **Never sends watch state by default.** §5 is opt-in because it names people.
- **Never blocks playback on an integration.** Every outbound call is queued or
  cached; monarr being down costs you a rail and a queued notification.

## Honest gaps, as of 2026-07-27

Recorded rather than quietly fixed, because a reader who knows the gap can
work around it and a reader who doesn't will misread the UI:

- **The Coming soon rail fails silently** (§3). Absence of the rail is not
  evidence of an empty calendar, and Test connection does not cover the
  calendar path.
- **plurx surfaces no inbound integration state in its UI.**
  `GET /api/v1/system` returns `notifications_received`, `scans_by_trigger`,
  `last_notification_at`, `last_notification_source`, `last_correlation_id`
  and `scan_requests` — and nothing renders them. Today the only way to see
  that monarr is calling is the Logs card or `/metrics`. This is the mirror of
  the gap monarr's inbound caller registry was written to close.
- **Library scan status does not name its trigger.** A scan shows the same
  way whether you clicked it or monarr asked for it, even though the request
  record carries `source` and `correlation_id`.
- **No keys UI** (§2). Minting is a curl.

## Keeping this honest

Every literal in this document — result strings, scope names, wire fields,
metric names — was read out of the code on the branch that ships it. When a
seam changes, this file changes in the same commit; a doc that is confidently
wrong about a wire format costs more than no doc at all.
