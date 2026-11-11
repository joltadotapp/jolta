---
version: 1
slug: "site-public-index-html"
primary_target: "site/public/index.html"
related_targets: ["site/public/styles.css"]
---

## Scope

The jolta.app homepage (`site/public/index.html`). Mode: Persuade. The legal pages share its stylesheet and theme but stay plain reading pages.

## Audience and action

Mac mail power users (Superhuman, Spark, Mimestream, Apple Mail). They should believe Jolta is fast and that no server of ours sits between their Mac and their provider. Action: Download for Mac. Proof: the live mini inbox, the measured numbers, and the app's own inbox-zero scenery.

## Constraints

- Light and dark themes. The default follows the system; a footer switch offers System, Light and Dark.
- Scripts only from jolta.app itself (CSP `script-src 'self'`); no inline script, no third-party requests. Fonts self-hosted (Gloock, Hanken Grotesk, OFL).
- `/updates/`, `/download/`, `/privacy`, `/terms-and-conditions` and `/.well-known/microsoft-identity-association.json` keep working.

## Direction contract

THESIS: The app's own empty-inbox sky becomes the page. It follows the visitor's clock (dawn, day, dusk, night) and theme, with painted hill planes that move at different speeds. It refuses the category default: a dark gradient hero with a glowing screenshot.

OWN-WORLD: Sky gradients and hill colors from `apple/Jolta/Scenery.swift` (Graphite Light skies; Graphite Dark mixes the same hues into #121214 and #0C0C0E). One amber bolt (#FFC020 to #FF8A00) for the mark and the inbox-zero strike. Gloock for display, Hanken Grotesk for text. Stars scattered at random, never in rows. Below the ridge, the hill dissolves into ground: pale hill tint in light, deep hill tone in dark.

STORY: A visitor learns that Jolta is fast keyboard-first mail with nobody in between. They try J, K and E in the live window, reach inbox zero, see the sky, and download.

FIRST VIEWPORT: Hairline bar with the bolt wordmark, links and the hour switch. A three-line serif headline at the left, with the lede, Download for Mac button and system line at the right. The live Graphite window is centered below, set on the hills, and the near ridge passes under it. Key hints sit on the ridge.

FORM: Pick card "Sky by the hour", merged with the dealt challenger "multiplane cel-animation dawn". It was first on my ordered list. Seed key a3eb05d9.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
