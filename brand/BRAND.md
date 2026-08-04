# noirr_ — suite brand guide

**Status:** selected · **Products:** Cinema, Curator, Runner

`noirr_` is the parent brand for three self-hosted media applications. The
underscore is part of the visual wordmark, not part of spoken product names.

| Public name | Current project | Job |
|---|---|---|
| **Noirr Curator** | `monarr` | Finds, evaluates, imports, and organizes media |
| **Noirr Runner** | `nzbd` | Downloads, repairs, extracts, and hands off files |
| **Noirr Cinema** | `plurx` | Serves the library and plays it on every screen |

Repository names, executables, API paths, bundle identifiers, URL schemes,
service-discovery names, environment variables, and data directories stay
unchanged until a separate compatibility migration is designed. Public
presentation can change without breaking an existing installation.

## Naming — one brand, three clear products

Use `noirr_` for the visual parent wordmark. Use **Noirr** in prose and store
metadata, where punctuation should not make the name harder to read aloud.

- Lockups: `noirr_ cinema`, `noirr_ curator`, `noirr_ runner`.
- Product prose: Noirr Cinema, Noirr Curator, Noirr Runner.
- Short in-app labels: Cinema, Curator, Runner.
- Pronunciation: “nwar.”

Do not use the retired working names `cinemarr`, `watch`, `fetch`, `screen`,
`Catalog`, or `Boundlight` in new public material. Historical technical names
such as `plurx`, `monarr`, and `nzbd` remain valid when they identify a binary,
configuration key, network service, or compatibility surface.

## Marks — related silhouettes with one shared signal

Every app icon uses a carbon field, a warm-white geometric letterform, and one
crimson dash. The dash is the shared suite signal; its position belongs to the
letter rather than floating at a fixed global coordinate.

| Product | Construction | Dash position |
|---|---|---|
| Cinema | Open uppercase C | Lower counter |
| Curator | Lowercase c beside a baseline-aligned uppercase U | Beneath the c |
| Runner | Geometric uppercase R | Between the stem and diagonal leg |

Keep the icon field solid and the letterforms flat. Do not add gradients,
shadows, film grain, scanlines, outlines, or a second accent. Platform masking
supplies corner radius; the master artwork remains a full square.

At small sizes, preserve the silhouette before preserving exact stroke width.
The 48 px crimson dash is the minimum master proportion and should not be made
thinner in downstream exports.

## Color — dark room, warm light, one red signal

| Token | Midnight | Matinee | Role |
|---|---|---|---|
| Background | `#0a0a0c` | `#f2efe8` | App field or warm paper |
| Ink | `#ededef` | `#1a1a1e` | Marks and primary type |
| Muted | `#9a9aa3` | `#5d5c63` | Product name in lockups |
| Accent | `#e5484d` | `#c2343a` | Underscore and icon dash |

The accent identifies the brand and active state. Error states use coral or a
separate semantic token so a red brand element never implies failure by
itself.

## Type — system voice and content voice stay distinct

Use JetBrains Mono 700 for the `noirr_` lockup and product name. Use JetBrains
Mono 500–700 for headings, labels, and technical values. Use Inter 400–600 for
long-form interface copy. Native Apple and Android clients may use platform
text styles for accessibility; the lockup remains monospace artwork.

Lockups are lowercase and use a single space between the red underscore and
the product name. The product name uses muted ink so the parent brand leads.

## Interface — artwork carries color

The shell stays restrained: dark neutral surfaces, warm-white text, and the
single crimson accent. Posters, covers, and backdrops supply most of the color.
Playback drops to true black so the brand never tints the picture.

Motion communicates state. Use short fades and direct transitions; avoid
bounce, decorative scanning, or an ambient blinking underscore. Reduced-motion
users receive the same information in a static state.

## Voice — concise and observable

Use direct language that says what happened and what the user can do next.
Lowercase flavor is welcome in a short empty state, but controls, errors, and
accessibility labels prioritize clarity over character.

- Good: `no releases matched this profile.`
- Good: `download paused — resume when the provider is reachable.`
- Avoid: jokes that hide the failure, blame another project, or replace an
  actionable explanation.

## Assets — masters first, generated exports second

Each repository owns its product icon master and generated export set under
`brand/`. The `icon-master.svg` file is the canonical geometry. Lockup SVGs are
self-contained and are the canonical typography. PNGs in `brand/export/` and
platform asset folders are generated outputs.

Before release, verify the three icons together at 16, 32, 64, 180, 512, and
1024 px; check Apple, Android adaptive, PWA maskable, and television crops; and
run a separate legal, trademark, domain, and package-name clearance. Visual
selection is not name clearance.
