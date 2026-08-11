# Client connectivity — what every client says when the server is gone

Companion to [CLIENTS.md](CLIENTS.md) (what the clients are) — this is *what
they do when they cannot reach plurxd, and why they all say the same thing.*

Before this document, all three clients rendered their platform's raw transport
string. The web app showed **"Failed to fetch"** on a dashed empty box. Android
showed `Unable to resolve host "plurx-ab12cd.local": No address associated with
hostname`. Apple showed *"Could not connect to the server."* — Foundation's
sentence, not ours. Three different vocabularies for one condition, none of them
telling the viewer what to do next, and on two of the three a network failure
during sign-in was reported as a wrong password.

The fix is one taxonomy, one copy table, and one rule: **a native error string
is diagnostics, never user copy.**

## 1. The taxonomy

Seven classes. Every transport or server failure lands in exactly one, and the
seventh exists so nothing can fall through to a native string.

| Class | The condition | Whose fault |
|---|---|---|
| `offline` | The device has no network. Nothing was sent. | This device |
| `unreachable` | Network is up; the server refused, is down, or has no route. | The server |
| `unknown_host` | The address does not resolve. | The address |
| `timeout` | Attempted, but no answer before the deadline. | The server |
| `insecure` | TLS failed — untrusted, expired, or mismatched certificate. | The certificate |
| `server_error` | Reached and answered, with 5xx or an unusable body. | The server |
| `unknown` | The fallback. | Unknown, and we say so |

`401`/`403` is **not** in this table. It has always had its own path — sign out,
re-authenticate — and folding it in here would make an expired token look like a
network problem. See §4.

The copy for each class lives in
[`tests/contracts/connectivity-copy.json`](../tests/contracts/connectivity-copy.json),
not in this document, because four test suites read that file and none of them
can read prose. This document explains the mapping; the JSON is the answer key.

Each class carries four strings and an action list:

- **`title`** — the headline for a full-surface error state.
- **`detail`** — one sentence saying what happened and what it implies.
- **`short`** — a single line for a toast, a snackbar, or an inline notice.
- **`actions`** — `retry` always, plus `change_server` when a different address
  is a plausible fix. `unreachable`, `unknown_host` and `insecure` earn it; a
  timeout or a 500 does not, because changing the address when the server is
  merely busy loses a working configuration.

`{server}` interpolates the server's display name, falling back to the origin,
falling back to `the server`. No title *begins* with `{server}` — "the server
isn't responding" as a headline reads like a typo, which is why the timeout
class is titled *No answer from {server}* rather than *{server} isn't
responding*.

## 2. Mapping native failures onto the classes

Each client owns one classifier function and nothing else may construct user
copy. The mapping is where the platforms genuinely differ, so each is written
out in full.

### 2.1 Web (`crates/plurxd/src/web/connectivity.js`)

The browser is the worst-informed of the three: `fetch()` collapses connection
refused, DNS failure, TLS failure and a CORS rejection into one opaque
`TypeError`. So the web classifier reads what it *can* observe and refuses to
guess what it cannot.

```
 request threw?
   │ no  ──▶ res.ok?  ── yes ──▶ (success)
   │             │ no
   │             ├── 401/403 ─────────────▶ (auth path, not a class)
   │             └── 5xx / bad body ──────▶ server_error
   │ yes
   ├── AbortError from our deadline ─────▶ timeout
   ├── navigator.onLine === false ───────▶ offline
   └── TypeError ────────────────────────▶ unreachable
```

`unreachable` is deliberately the catch-all for a live-network `TypeError`,
because it is the *most common* true cause and its copy — "the server may be
powered off, restarting, or on another network" — stays honest even when the
real cause was DNS or TLS. Claiming `unknown_host` from evidence the browser
never supplies would be a confident lie. `navigator.onLine === false` is
trustworthy in the negative direction only (a `false` means no interface;
`true` means very little), which is exactly how it is used.

### 2.2 Android (`clients/android/.../data/Connectivity.kt`)

OkHttp throws typed exceptions, so the mapping is direct. Order matters —
`SSLException`, `ConnectException` and `UnknownHostException` are all
`IOException`s, so the general case must come last.

| Throwable | Class |
|---|---|
| `SSLException`, `SSLHandshakeException`, `CertificateException` | `insecure` |
| `UnknownHostException` | `unknown_host` |
| `SocketTimeoutException`, `InterruptedIOException` (call deadline) | `timeout` |
| `ConnectException`, `NoRouteToHostException`, `PortUnreachableException` | `unreachable` |
| `HttpException` 5xx, `SerializationException`/parse failures | `server_error` |
| any other `IOException` | `unreachable` |
| anything else | `unknown` |

`offline` is not derived from an exception — every one of the above can occur
with the radio off. It is decided first, from `ConnectivityManager`'s active
network: **no validated network ⇒ `offline`, whatever the exception says.**
Checking the system before trusting the exception is what turns "Unable to
resolve host" into "You're offline" on a plane.

### 2.3 Apple (`clients/apple/Sources/Connectivity.swift`)

`URLError` carries the richest codes of the three, and the previous client
inspected exactly one of them (`.cancelled`).

| `URLError.Code` | Class |
|---|---|
| `.notConnectedToInternet`, `.internationalRoamingOff`, `.callIsActive` | `offline` |
| `.cannotFindHost`, `.dnsLookupFailed` | `unknown_host` |
| `.timedOut` | `timeout` |
| `.secureConnectionFailed`, `.serverCertificate*`, `.clientCertificate*`, `.appTransportSecurityRequiresSecureConnection` | `insecure` |
| `.cannotConnectToHost`, `.networkConnectionLost`, `.cannotLoadFromNetwork`, `.resourceUnavailable`, `.badServerResponse` | `unreachable` |
| `.cancelled` | not a class — stays `CancellationError` |
| any other `URLError` | `unreachable` |
| `APIError.http(5xx)`, decoding failures | `server_error` |
| anything else | `unknown` |

**`waitsForConnectivity` stays `true` on the API session, and Apple therefore
reports a fully offline device as `timeout`, not `offline`.** This is a
deliberate trade and it is the one place the three clients genuinely diverge.

Flipping it to `false` is tempting — it is what makes `.notConnectedToInternet`
surface at all, and without it the `offline` class is nearly unreachable on
Apple. It is also a regression. The flag was added to cover the local-network
permission sheet: on a fresh install the first request is made while iOS is
still asking the user to allow local networking, and a session that does not
wait fails underneath the sheet, so connecting works only on the second tap.
The Bonjour preflight in `AppModel.bootstrap()` existed *before* that flag and
was judged insufficient — the flag is the fix, not redundant with it.

So the deadline does the work instead: `timeoutIntervalForResource = 30` bounds
the wait, and an offline device gets *No answer from the server* after 30
seconds rather than a spinner forever. Honest, if less precise than Android's
answer. Making it precise needs `NWPathMonitor`, which is a §6 non-goal.

## 3. Deadlines — nothing may hang

An error state a user never reaches is worse than a bad error message. Before
this work the web client had **no request timeout at all**: a blackholing host
(wrong subnet, firewall dropping instead of rejecting) left `Loading…` on screen
forever, with no error to classify because nothing ever failed.

| Client | Ordinary API call | Playback preparation | Why they differ |
|---|---|---|---|
| Web | 15 s (`AbortSignal.timeout`) | 120 s | A decision or session open can legitimately wait on ffmpeg probing a 4K remux |
| Android | 30 s `callTimeout` on a **separate** API client | 180 s (`/decision` **and** session open); media transfers keep the deadline-free shared client | A `callTimeout` on the shared client would abort long segment and offline-package reads |
| Apple | 15 s request / 30 s resource | 180 s (`/decision` **and** session open) | `/decision` runs `ffprobe -show_chapters` server-side with no timeout of its own |

`/decision` deserves its own note, because it looks like an ordinary JSON call
and is not. Server-side it reaches `markers_for`, which can fall through to a
live `ffprobe -show_chapters` subprocess with no timeout, behind an availability
stat that may be sitting on a spun-down NAS. Both the web client and the Apple
client therefore give it the playback deadline, not the API one. A 15-second
deadline there turns a slow first play of a large file into *No answer from the
server* — an error state for something that was merely working.

The Android split is the subtle one, and it is a three-way split rather than
the two it first looks like. `Net.client` is handed to
`OkHttpDataSource.Factory` for playback and to the offline downloader; a call
deadline there would kill a 700 MB package mid-transfer, so it has none. The
API client is `client.newBuilder().callTimeout(30s).build()`, which shares the
connection pool and dispatcher — so this costs nothing but gives the JSON path
a deadline the media path must not have.

The third client is the one the first version of this work missed. `/decision`
and `POST /files/{id}/hls/sessions` are declared on the same `PlurxApi`
interface as `/hubs`, so a single Retrofit client hands them the 30-second API
deadline — which is *shorter* than the 60-second read timeout they had before
any of this, and is the paragraph above saying not to do exactly that. A first
play of a large remux on a sleeping NAS then aborts at 30 s with *No answer
from {server}* for a file that plays fine on the second press. So Retrofit gets
a `Call.Factory` that picks the client per route: those two paths get 180 s,
matching Apple, and everything else gets 30 s. `callTimeout` is a property of
the client rather than of the call, so an interceptor cannot make this choice —
it has to be made when the call is created.

## 4. Sign-in is not a credentials check

This was the most damaging of the old behaviors and it existed on all three
clients: a network failure during sign-in rendered *"Wrong username or
password"* (Android, verbatim) or the raw fetch error styled in the same red as
a credentials error (web, Apple). A viewer whose server was simply off would
retype a correct password until they gave up.

The rule, enforced by test on each client:

```
 sign-in failed
   │
   ├── HTTP 401 / 403 ──────▶ credentials_message, verbatim
   └── anything else ───────▶ classify() and render that class
```

`credentials_message` lives in the contract alongside the classes for the same
reason they do: it is the sentence a client says *instead of* a class, so a
client that words it differently has quietly reintroduced the split this
document exists to close.

The same rule governs launch. A saved session that fails to validate for
transport reasons must not drop the viewer at a login form implying their
session expired — it holds the credentials and shows the connectivity class
with a retry, which is what the Apple client's `reconnectFailed` phase already
did and the other two did not.

## 5. Where each class is rendered

Three surface shapes, and every client has all three.

**Full surface** — the view has no content to show. Renders `title`, `detail`,
and every action in `actions` the surface can actually perform. Web:
`renderConnectionError()` into `#main`, or into `#app` when there is no shell
yet. Android: `ConnectionErrorState` composable. Apple: `ConnectionErrorView`.

The web surface draws `retry` only, and deliberately: the page is served *by*
the server it is failing to reach, so "Change server" would reload the same
origin and change nothing. The action stays in the contract because the native
clients, which hold a configurable origin, do act on it. Each client asks a pure
function which actions it can draw rather than filtering inline, so the omission
is a tested decision instead of a missing loop body.

**Transient notice** — the view already has content and a refresh failed.
Renders `short` only, does not disturb what is on screen. Web: `toast()`.
Android: an inline banner above the content. Apple: the same. This is
`cached_content_wins` in the contract, and it is why a poll failing every few
seconds no longer repaints an error over a working page — the web activity view
used to redraw "Failed to fetch" on a 3-second timer.

**Inline field** — a form the user is filling in. Renders `short` beneath the
form, with the retry action being the form's own submit button. Web: `.err`
divs. Android: `AuthScaffold`. Apple: `AuthScaffold`.

## 6. Non-goals

Named so they do not get built by accident:

- **No automatic reconnection.** Nothing polls `/healthz`, nothing retries on a
  backoff, no banner clears itself when the server returns. Every recovery is a
  deliberate tap. Auto-recovery is a real feature and it is a separate one; it
  needs a reachability monitor per platform and its own acceptance evidence.
- **No offline degradation.** When the server is unreachable the clients do not
  fall back to cached library content. Downloaded titles remain playable
  because the offline stack never depended on the server, but that is existing
  behavior, not new fallback.
- **No retry inside the classifier.** Classification is a pure function of the
  failure. The one pre-existing retry loop — Android's
  `validateSavedSession` launch backoff — is untouched.
- **No new copy outside the contract.** A screen needing a sentence the JSON
  does not have is a signal the taxonomy is short a class, not a licence to
  write a string.

## 7. Keeping it honest

The claim this work makes is *every client says the same thing*, and that is
exactly the kind of claim that rots. It is pinned by four suites reading the
same JSON:

| Test | What it proves |
|---|---|
| `crates/plurxd/src/http/web.rs` | The shipped `connectivity.js` names every class in the contract, and loads before the app script that uses it |
| `tests/connectivity/web-copy.test.js` | The web classifier's output for each class is byte-identical to the contract, including `{server}` interpolation |
| `ConnectivityCopyTest.kt` | The Android classifier maps every real exception type to the right class and emits contract copy |
| `AppleClientTests.swift` | The Apple classifier maps every `URLError.Code` in §2.3 and emits contract copy |

Each also asserts the negative: that no classifier output contains the native
error text it was given. A test that only checks the happy strings would pass a
client that appended "(Failed to fetch)" to every one of them.

Run them with `make web-check` (web), `make android-test`, and `make
apple-test`; all three are in the CI fan-out already.
