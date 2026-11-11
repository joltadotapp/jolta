---
version: 1
slug: "site-public-features-index-html"
primary_target: "site/public/features/index.html"
related_targets: ["site/public/features.js"]
---

## Scope

The jolta.app features page (`site/public/features/index.html`). Mode: Persuade. It extends the homepage's world (DESIGN.md) and adds no new visual system.

## Audience and action

Mac mail power users who want to see the app before downloading. They should see the real app respond to the keys they would press. Action: Download for Mac. Proof: recordings of the demo copy of Jolta on invented mail, in Graphite Light and Graphite Dark.

## Constraints

- Recordings are H.264 MP4 under `site/public/media/features/`, made with the jolta-demo skill at the compact window size; phones get crops of each one (`-phone`). The Worker answers byte ranges for `/media/` because Safari needs them.
- Clips autoplay muted and loop only while on screen; a Pause and Play button is always present; with reduced motion they wait behind a Play button.
- No calendar or AI recording until the demo data has events and an AI provider.

## Direction contract

THESIS: One stage shows the real app doing one feature at a time, and the app's own keys pick the feature. It refuses the alternating clip-and-text rows every product page uses.

OWN-WORLD: The homepage's world: the hour-following sky header with stars and hill planes, the ridge dissolving into ground, Gloock headlines, Hanken text, keycaps drawn like the homepage's, the closing sky band. The recordings carry Graphite, matched to the page theme.

STORY: A visitor picks E, ↩, /, ⌘K, H or C, on screen or on their keyboard, watches the real app do it, reads what else is in the app, and downloads.

FIRST VIEWPORT: The short sky header with the two-line headline at the left and the lede and Download for Mac at the right. Below the ridge, the keycap tab row, then the clip stage with the text panel to its right.

FORM: "One stage, keys as tabs", dealt as the lead of the surface roll and chosen by the owner. Seed key 8d4b1fb6.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
