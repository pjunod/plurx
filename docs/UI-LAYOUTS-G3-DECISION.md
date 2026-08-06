# G3 — did the layout abstraction pay for itself?

**Status:** decided · **Verdict: revise the contract, then continue** ·
**Gate:** G3 of [UI-LAYOUTS-IMPLEMENTATION.md](UI-LAYOUTS-IMPLEMENTATION.md)
· **Measured against:** `crates/plurxd/src/web/index.html` at 693,806 bytes ·
**Written:** 2026-08-03

Companion to [UI-LAYOUTS-STATUS.md](UI-LAYOUTS-STATUS.md) (what got built) —
this is *whether it was worth building*, and what has to change before a
second layout starts. Every number here was re-derived at the moment of
writing; §1 says why that sentence is in this document at all.

The short version: the composition thesis holds — `catalog` is paint over one
application, not a second application — and the **verification** contract
does not. It failed twice on the easiest layout this arc will ever build,
and G4 doubles the surface it cannot see. Three conditions must land before
Theater starts. Two of them landed with this record.

---

## 1. The evidence

| G3 asks for | Measured |
|---|---|
| duplicated markup | **0** new card / rail / grid / fetch builders in `catalog` |
| extra requests | **0** — 18 route-captures, `classic` vs `catalog`, identical |
| source size delta vs the +60–90 KB budget | **+112.0 KiB (+19.8%)** for ONE layout + seven themes |
| route regressions caught by `ui-baseline` | **0 real, 1 false positive** — and it could not see the two that mattered (§3) |
| cost of one shared card fix under both layouts | **24 added lines, 2 of them layout-scoped** |

**How to read the size number.** 693,806 bytes is the whole file, and 196,292
of that (28.3%) is three base64 `@font-face` lines that have not changed all
arc. On the part that is actually code, the growth is **+30.0%** — a worse
number than the headline, and the honest one. Roughly half the new JS is
comment prose; that is a house-style choice this document is making explicit
rather than leaving to be discovered at G6.

**Both earlier size figures in this arc were stale the day they were
written** — STATUS §3.7's +65.9 KB was 41 KB out of date by the time this
gate opened. Re-derive at the moment of the record; a size claim ages faster
than anything else here.

## 2. The abstraction, judged

**It is real.** `catalog` composes `card()`, `rail()`, `grid()`, `comingCard()`,
`installBannerHtml()`, `groupToggleHtml()` and re-emits classic's `<select>`
markup unmodified. It reuses the theme, appearance, size and profile popovers
rather than forking them. It fetches nothing: the sidebar's data problem is
solved by paint-then-fill against the existing cache, not by a chrome-level
request — which is why the request-log invariant passes without an enforcer
having existed while the code was written.

**The shared-card change is the number that settles it.** Teaching every
layout to ask about watch state rather than infer it from absent children
cost 24 added lines, of which **2** were layout-specific. 92% of the change
was in shared code and served both layouts at once. Under a
five-applications-in-a-trenchcoat architecture that same fix is five edits
and five chances to miss one.

**But "zero new builders" measures the wrong layer.** `catalogItemBody`
re-derives roughly 70 lines from `classicItemBody`: the show/season year
range, the resume threshold, the playable-file pick, the multi-versus-missing
version-header rule, the Contents/Seasons/Episodes label. No component was
duplicated; the *decisions* were. S3's Genre pill is already a two-site edit
and will be five by G6. That is R4 below, and it is the difference between an
abstraction that holds and one that is holding.

### The counter-evidence worth naming

One shared rule — `.azbar button.active{color:#fff}` — was defective on pale
accents, and it was corrected with a `[data-layout=catalog]` override rather
than at the source. That is the abstraction becoming two applications by a
route nobody was watching: the shared rule keeps the defect and each layout
grows its own correction. Fixed at the source in this gate's commit set.
`contrast-check` audits the `THEMES` table and structurally cannot see a
literal colour inside a CSS rule, so this class of bug had no gate at all.

## 3. What the byte-identical baseline actually protects

This is the most valuable finding of the gate, and it is not a flattering
one.

`ui-baseline` asserts exactly this: **`render(route)` is a fixed point over 9
routes × 2 viewports, for one `(layout, theme, appearance)` triple, at first
paint, on a six-item library, with no user interaction**, plus the first N tab
stops. It is a *conservation* check, not a *presence* check.

```
 sees                                    cannot see
 ────                                    ──────────
 accidental edits to shared render       anything behind an interaction
   code that reach classic's paint         (click, scroll, select, nav)
 boot exceptions, console errors         anything under a non-default layout
 gross palette/box regressions           19 of the 20 theme × mode palettes
   in the one palette photographed       anything past the first batch
 tab-order changes                       semantics behind identical pixels
                                         comment/code divergence
```

Two G2b fixes were silently reverted by a background job and shipped as
done. **The harness was not broken when it passed them.** A fix that only
manifests under `catalog` is, from `classic`'s point of view, a correct no-op —
the harness answered precisely the question it was asked. The error was
treating "classic is unchanged" as a proxy for "the change landed." They are
different assertions and only one of them was being made.

And the coverage **shrinks as the arc succeeds**: today the baseline
photographs 1 of 2 layouts; at G6 it photographs 1 of 5.

What caught the loss was grepping the shipped file for the class name. That
is now a rule, not an accident — R3.

## 4. The two calls this record must make

### 4.1 Re-baseline the size envelope

The +60–90 KB budget was for the whole five-layout system. One layout and a
theme catalogue spent **1.2× the entire budget**. Decomposed:

| Component | Bytes |
|---|---:|
| `catalog` JS | ~41,000 (about half of it comment prose) |
| `catalog` CSS, 219 scoped rules | ~26,000 |
| seven new theme tables + treatments | ~12,900 |
| registry, seam, hooks | ~5,400 |
| greyed-control machinery | ~2,400 |

So the **marginal cost of a layout is ~67 KB**, and ~48 KB of the total was
one-time: the seam, and a theme catalogue that is now finished.

**Do not scale that linearly — the correction runs upward.** `catalog` is the
cheapest layout that will ever ship. It converted 3 of 7 routes, inherited
the library `items`/`count` regions, the pager and the rail wholesale, added
zero components, and left activity, settings, search and player to classic
behind its chrome. Deck cannot do any of that: an operator console is
table-first, so it needs a table component — the "no new builders" line
breaks at G5 **by design** — and it must convert activity and settings, which
nobody has priced. Guide needs a schedule grid and a D-pad focus model.

| Gate | Plan v4 estimate | Re-baselined | Why |
|---|---|---|---|
| G4 Theater | +35–55 KB | **+45–65 KB** | same three routes; no sidebar/tabs, heavier hero CSS |
| G5 Deck | +60–90 KB | **+95–135 KB** | new table component · activity + settings conversion · S1/S2 wiring |
| G6 Guide | +60–90 KB | **+85–125 KB** | schedule grid · D-pad model · channel construction |
| **total by G6** | ~820–910 KB | **~920–1,020 KB** | of which ~100 KB is documentation |

### 4.2 Single file, or split?

**Split the fonts out now. Do not split per-layout yet. Revisit at G5.**

The fonts are 28.3% of the file, three base64 lines, and nobody hand-edits
them. `crates/plurxd/src/http/web.rs` already serves five `include_str!` /
`include_bytes!` assets, so moving them is a small Rust change with no build
step and no new failure mode. It removes more than a quarter of the file from
every diff, every `js-check` read, and every context window that has to hold
it.

Per-layout chunks are the wrong answer *today*, and the evidence points away
from the plan's own framing of them. The stated benefit is parallelism
against the single-owner rule — but the conflicts in this arc were all in
**shared** code: `card()`, `homeCard()`, `stickyFloor()`, `invalidateLibs()`,
`libsCached()`, `setLayout()`, `alphaRailHtml()`, and the entire library
route. Splitting by layout parallelises none of that; it converts "one owner
of `index.html`" into "one owner of `web/`" plus a manifest to get wrong. And
a chunk somebody forgot to register renders as a stylesheet that failed to
load — already the hardest symptom in this codebase to debug, and the one
that bit the `catalog` port.

Trigger to revisit: Theater lands ≥40 KB of layout-scoped JS **and** two
layout gates need to run concurrently. Split by *kind* first (`styles.css`,
`app.js`) rather than by layout — that alone turns `js-check` into
`node --check app.js` with real line numbers, which is strictly better
tooling than exists now.

## 5. Verdict — revise, then continue

Not *stop*: the seam holds, `catalog` is 67 KB of paint rather than a second
application, and the composition thesis survived contact at the component
level. Not *continue* unchanged: the verification contract failed twice on
the easiest case, and G4 doubles what it cannot see.

Five revisions. **R2 and R5 landed with this record**; R1, R3 and R4 are
entry conditions for G4.

- **R1 — the baseline must cover every shipped layout, and must compare.**
  `make ui-baseline` captures and exits; comparison is a manual `sha256sum`
  in a doc. It must capture every registered layout and fail on drift, and it
  must run in `scripts/pre-commit` and CI. Without this, G4 walks into the
  exact hole that shipped the lost fixes, with two non-default layouts to
  miss instead of one. **Not done.**
- **R2 — implement the request-log invariant, or delete the claim.** It was
  cited in four places as the enforcer of §3.2's fetch discipline and had no
  code behind it. **Done:** `ui-baseline` now records method + path per route
  and writes `requests.json`. First run of it: 18 route-captures, `classic`
  vs `catalog`, **0 differing**. The claim was true; it just was not being
  checked.
- **R3 — every gate's accept list gains a presence assertion.** Not only
  "classic is unchanged" but "the new behaviour is in the artifact" — grep
  the shipped file for the class, selector or attribute. And §9.3's
  independent verifier reads the artifact, never the note. **Not done —
  a process change for the plan.**
- **R4 — extend "no second card function" to route bodies.** Move the shared
  derivation in `classicItemBody` / `catalogItemBody` — chips, resume, playable
  file, version headers, children label — into `loadItem()`'s page model, so
  a body builder composes rather than decides. One commit now; four at G6.
  **Not done.**
- **R5 — settle `LIB_SORT`.** Three call sites disagreed about stored versus
  effective sort. **Done:** `tagAlpha` now takes the effective sort like
  `alphaRailHtml` does. It had been half-fixed, which was worse than not
  fixed: the A–Z rail rendered every letter, tagged no card, and jumped
  nowhere — on every fresh session, in both layouts, photographed perfectly
  by the golden.

## 6. What this gate cost, and what it bought

It cost about 112 KB of source and a verification harness that turned out to
be answering a narrower question than anyone was reading it as.

It bought a boundary that a second layout could be built behind without
forking a fetch, a component, or a route; a shared fix that costs 24 lines
instead of five edits; a contrast gate that can only shrink; and — via the
two lost fixes and the half-applied rail — a precise, unflattering map of
what this project's automated checks do and do not see. That map is worth
more than the layout.
