# noirr — brand guide v0.2

**Sonarr scanned the air. Radarr swept the sky. noirr works in the dark.**

One name for the whole stack: the Sonarr+Radarr rewrite, the NZBGet rewrite, and the media server. Your library, remuxed.

## the name

- **noirr** — noir (the genre, the dark) spoken with the arr-family accent. The double-R tells the self-hosted crowd exactly what this is; the word tells everyone else how it feels.
- **Pronounced "nwar."** The last letter is silent, like the sonar in Sonarr.
- Always lowercase, everywhere: `noirr`, never "Noirr" or "NOIRR" (headlines included).
- Uniquely searchable — no software shares the string. Release tag: `-NOIRR` (caps allowed here only, scene convention).

## wordmark

`noirr_` — JetBrains Mono 700, tight tracking, lowercase.

- **The cursor is the status light.** Blinking = the system is working; solid = idle. Omit it in print/static contexts.
- Cursor is always accent-colored; letters are always ink. In midnight it glows; in matinee it's red ink — no glow.
- Don't outline, gradient, or italicize the letters. Don't add a second color.
- Files: `wordmark.svg` / `wordmark-dark.png` (midnight), `wordmark-ink.svg` / `wordmark-matinee.png` (light), transparent PNGs of both.

## suite lockups

Functional subcommands, dimmed, after the brand:

- `noirr watch` — library manager (the Sonarr+Radarr rewrite): tracks the library, tails releases
- `noirr fetch` — the downloader (NZBGet rewrite): brings the package in
- `noirr screen` — the server: runs the picture

Bench words if any ever need to change: index, haul, play.

## icon

**A — `n_`** (selected): pathed lowercase mono n + glowing cursor on a dark scanlined tile. Benched alternates, kept in the kit: B — the bare cursor block; C — light-through-blinds cutting an N. All in `icon-*.svg`, no font dependency. **The icon stays midnight in both themes** — the app is the dark room you walk into.

Production set in `icon/`: PNGs at 512/256/128/64/48/32/16 + `favicon.ico`. Sizes ≤32px use `icon-a-favicon.svg` — a simplified cut (thicker stroke, no scanlines, no glow) so it stays crisp.

## themes — showtimes

Named like showtimes. Semantic tokens remap; components never change.

| theme | role | story |
|---|---|---|
| **midnight** | dark · default | the terminal at night — neon glow, film grain |
| **matinee** | light | daylight noir — warm paper, ink, red-ink cursor, paper grain |
| projection | mode, not theme | fullscreen playback drops to true black in ANY theme — you watch in the dark |
| giallo / silver | reserved | amber accent / pure B&W — someday |

Selection: `<html data-theme="midnight|matinee">`, auto via `prefers-color-scheme`, manual override wins.

### midnight palette

| token | hex | role |
|---|---|---|
| oled | `#050506` | projection mode |
| bg | `#0a0a0c` | app background |
| surface | `#101014` | cards |
| raised | `#16161b` | hover / elevated |
| line | `rgba(255,255,255,.08)` | borders |
| ink / dim / faint | `#ededef` / `#9a9aa3` / `#5c5c66` | text ramp |
| accent | `#e5484d` | cursor, active, progress (glows) |
| accent-deep | `#8e1c22` | pressed / fills |
| ok / warn / err | `#5fb582` / `#d9a05b` / `#ff7a66` | err is coral, never brand crimson |

### matinee palette

| token | hex | role |
|---|---|---|
| bg | `#f2efe8` | the desk — warm paper, never blue-white |
| surface | `#faf8f2` | the sheet |
| raised | `#ffffff` | elevated |
| line | `rgba(24,22,20,.12)` | borders |
| ink / dim / faint | `#1a1a1e` / `#5d5c63` / `#908f95` | text ramp |
| accent | `#c2343a` | red ink — no glow, shadows do the work |
| accent-deep | `#8e1c22` | shared with midnight |
| ok / warn / err | `#35855c` / `#a3742b` / `#c96442` | err is terracotta, distinct from red-ink |

Theme rules: **in daylight, nothing glows** (shadows replace glow) · **playback is always midnight** · **posters carry the color in both rooms**.

## type

- **JetBrains Mono** (700/500) — brand, headings, labels, and every technical string (codecs, sizes, release names).
- **Inter** (400–600) — body copy, descriptions, settings.
- Mono is the voice of the system; the sans is the voice of the content.

## shell principles

1. **light has a source** — gradients are directional, glows come from something
2. **posters are the color** — UI monochrome, artwork carries saturation
3. **scanlines are the loading language** — skeletons and shimmers are horizontal, always
4. **grain, sparingly** — 3–5% film grain (midnight) or ~3% paper grain (matinee), never over text
5. **cuts, not slides** — fast fades and hard cuts; nothing bounces in a noir
6. **the cursor is the status light** — blinking = working, solid = idle

## voice

Deadpan, terse, lowercase. Flavor lives only in empty states and errors — never in the way of a task.

- empty library: `// nothing on the shelf yet`
- failed grab: `the tail went cold. retrying in 30s_`
- 404: `this reel doesn't exist.`

## ship list (public release)

- [ ] `noirr.tv` — appeared unregistered at check (2026-07-19); grab first
- [ ] `noirr.app`
- [ ] GitHub org `noirr-media` (matches socials; bare `noirr` is a squatted, empty account)
- [ ] npm scope `@noirr`
- [ ] socials `@noirrmedia` — "media" covers the whole suite, not one slice
