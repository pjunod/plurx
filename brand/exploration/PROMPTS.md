# Exploration prompts — how the two concept boards were made

Companion to [EXPLORATION.md](../EXPLORATION.md). These are the final built-in
image-generation prompt sequences behind the two PNG concept boards. They are
records for iteration, not production logo specifications.

## Identity board

```text
Use case: logo-brand
Asset type: exploratory brand identity board, 16:9 landscape
Primary request: Create a sophisticated vector-style identity concept board
for a local-first, self-hosted media software suite named "boundlight". The
software is fast, precise, durable, owner-controlled, observable,
cluster-capable, and consists of three tightly integrated apps.
Brand architecture: one master brand, with three products labeled exactly
"CATALOG", "FETCH", and "SCREEN".
Subject: At top, a clean lowercase wordmark "boundlight". Develop one bold
master symbol based on the idea of light held within a boundary: a compact
geometric B-like or bracket-like silhouette containing one continuous luminous
line. Below it, show three coordinated app icon variants derived from exactly
the same geometry, labeled "CATALOG", "FETCH", and "SCREEN". The product
variants should suggest organizing, transferring, and viewing through subtle
geometry, not literal clip-art.
Style/medium: premium flat vector identity presentation, Swiss/editorial
restraint with modern systems-software precision; strong silhouette, excellent
negative space, legible at Apple TV icon scale; polished design-studio concept
sheet.
Composition/framing: spacious 16:9 board on warm off-white; wordmark and hero
mark at top/left, icon family centered, small palette and typography samples at
bottom; rational grid; generous margins.
Color palette: carbon #0B0D10, bone #F2EEE5, electric chartreuse #D8FF4F as the
shared signal color, electric blue #5877FF and warm coral #FF6B4A only as
restrained product identifiers.
Text (verbatim): "boundlight", "CATALOG", "FETCH", "SCREEN",
"everything between want and watch"
Constraints: render all text exactly once and spelled correctly; original
design; flat-color core marks; simple enough to redraw as SVG; master identity
must unify the three products; no other text.
Avoid: Plex-like orange chevrons, Sonarr/Radarr visual language, pirate
imagery, film reels, clapperboards, clouds, antennas, generic play-button or
download-arrow clip art, lowercase monospace terminal branding, cyberpunk,
neon overload, gradients in the core logo, 3D mockups, watermarks.
```

## Product UI board

The base generation:

```text
Use case: ui-mockup
Asset type: exploratory shared product UI design board, 16:9 landscape
Primary request: Design a polished, shippable-looking UI concept board for the
"boundlight" self-hosted media suite, showing how one design system adapts to
three tightly integrated products without forcing them into identical layouts.
Brand position: fast, precise, durable, local-first, owner-controlled,
observable, modern systems software.
Brand mark: use a compact black rounded B-like boundary containing a single
electric chartreuse circuit/light line, consistent with the earlier boundlight
identity concept.
Composition/framing: a clean 16:9 presentation board with two large interface
mockups side by side and a narrow shared pipeline/status strip underneath.
Left mockup: "boundlight catalog" desktop admin interface. Dense but calm left
navigation rail, global search, media library table/card hybrid, wanted items
and quality decisions, restrained cover art, contextual right-side details
drawer. Show one selected movie and a clear acquisition decision trace.
Right mockup: "boundlight screen" Apple TV / living-room interface. Large
poster rails, strong focus state readable from a sofa, minimal chrome, continue-
watching row, oversized focused card, artwork carries most of the color.
Bottom strip: exactly three connected stages labeled "CATALOG", "FETCH", and
"SCREEN", with a single item moving through them and compact health/state
indicators. This is the shared cross-app integration ribbon.
Style/medium: realistic high-fidelity product UI, not concept art; spatial,
editorial, understated; flat panels; excellent typography and spacing;
practical controls.
Color palette: carbon #0B0D10 and near-black surfaces, bone #F2EEE5 text,
electric chartreuse #D8FF4F as the shared active/signal color; electric blue
#5877FF and warm coral #FF6B4A used sparingly for product/state identification;
poster art supplies additional color.
Text (verbatim where visible): "boundlight catalog", "boundlight screen",
"Library", "Wanted", "Discover", "Activity", "System", "Continue watching",
"Next up", "CATALOG", "FETCH", "SCREEN", "Direct play", "Healthy"
Constraints: spell the brand and main labels correctly; show desktop admin and
10-foot TV modes as related siblings, not identical skins; accessible contrast;
status is conveyed by text plus color; no other company brands.
Avoid: Plex-like orange chevrons, Sonarr/Radarr layouts or colors, generic
streaming-service clone, film noir styling, terminal/hacker UI, excessive glow,
glassmorphism, huge empty dashboards, decorative charts, clouds, pirates,
watermarks.
```

The first output used recognizable media, so it received two narrow edits. The
first replaced titles and artwork throughout:

```text
Use case: precise-object-edit
Asset type: exploratory shared product UI design board, 16:9 landscape
Primary request: Edit the most recent boundlight UI concept board. Change only
all recognizable third-party movie/TV titles, character images, and poster/key
art to completely original fictional media. Keep the interface layout,
hierarchy, brand mark, palette, typography, panels, pipeline ribbon, and all
Boundlight product labels unchanged.
Replacement fictional titles (use only these where media titles are visible):
"Static City", "Dead Air", "Neon Harvest", "The Silent Reel", "Meridian",
"Nightshift", "Aftertaste", "Grain", "Last Projection".
Poster/art direction: original abstract editorial poster art using geometric
light, grain, shadow, architectural forms, silhouettes with no recognizable
actors, and no resemblance to existing entertainment key art.
Text to preserve exactly: "boundlight", "boundlight catalog",
"boundlight screen", "Library", "Wanted", "Discover", "Activity", "System",
"Continue watching", "Next up", "CATALOG", "FETCH", "SCREEN", "Direct play",
"Healthy".
Constraints: change only third-party media content; preserve everything else;
no real film, series, actor, studio, or franchise names; no recognizable
copyrighted characters; no logos or watermarks.
```

The second corrected five titles left in the `WANTED` panel:

```text
Use case: precise-object-edit
Asset type: exploratory shared product UI design board, 16:9 landscape
Primary request: Edit only the five rows in the small "WANTED" panel in the
lower-left area of the boundlight catalog interface. Replace the remaining real
movie titles as follows, preserving every other pixel, layout, panel, title,
image, and label unchanged:
"Heat" -> "Last Projection"
"True Romance" -> "Aftertaste"
"The Thing" -> "Grain"
"No Country for Old Men" -> "Nightshift"
"Children of Men" -> "Neon Harvest"
Constraints: change only those five text strings in the WANTED panel; render
the five replacement titles spelled exactly; no other changes; no real film,
series, actor, studio, or franchise names; no logos or watermarks.
```
