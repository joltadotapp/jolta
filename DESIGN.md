---
name: Jolta website
description: The jolta.app site. The app's inbox-zero sky, set by the visitor's clock and theme, is the page.
colors:
  sky-top: "#5FA8E8"
  sky-bottom: "#CFE8F7"
  hill-far: "#8DBF8A"
  hill-mid: "#76AD77"
  hill-near: "#4a784d"
  sky-ink: "#17304A"
  sky-ink-soft: "rgba(23,48,74,0.82)"
  hero-button: "#17181A"
  hero-button-ink: "#ffffff"
  page: "#eef4ee"
  text: "#17181a"
  text-soft: "#4a4c52"
  rule: "rgba(20,22,26,0.14)"
  keycap: "#ffffff"
  keycap-edge: "rgba(20,22,26,0.22)"
  focus: "#e5521a"
  bolt-amber: "#FFC020"
  bolt-light: "#FFE066"
  bolt-deep: "#FF8A00"
  bolt-edge: "#C86A00"
  ridge-ink: "#f4f6f2"
  sky-top-dark: "#1c2938"
  sky-bottom-dark: "#283b42"
  hill-far-dark: "#2b382c"
  hill-mid-dark: "#1f291f"
  hill-near-dark: "#131913"
  sky-ink-dark: "#ECECEE"
  hero-button-dark: "#F2F2F0"
  hero-button-ink-dark: "#121214"
  page-dark: "#121512"
  text-dark: "#ECECEE"
  text-soft-dark: "#a8a8b0"
  rule-dark: "rgba(255,255,255,0.13)"
  keycap-dark: "rgba(255,255,255,0.07)"
  keycap-edge-dark: "rgba(255,255,255,0.24)"
  focus-dark: "#ff6b2c"
typography:
  display:
    fontFamily: "Gloock, New York, Georgia, serif"
    fontSize: "clamp(3rem, 6.4vw, 5.4rem)"
    fontWeight: 400
    lineHeight: 0.98
    letterSpacing: "-0.02em"
  headline:
    fontFamily: "Gloock, New York, Georgia, serif"
    fontSize: "clamp(2.4rem, 5vw, 4.2rem)"
    fontWeight: 400
    lineHeight: 1
    letterSpacing: "-0.015em"
  title:
    fontFamily: "Hanken Grotesk, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "1.1rem"
    fontWeight: 600
    lineHeight: 1.3
  body:
    fontFamily: "Hanken Grotesk, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "1.0625rem"
    fontWeight: 400
    lineHeight: 1.6
  lede:
    fontFamily: "Hanken Grotesk, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "1.12rem"
    fontWeight: 400
    lineHeight: 1.6
  label:
    fontFamily: "Hanken Grotesk, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "0.82rem"
    fontWeight: 500
    lineHeight: 1.2
  wordmark:
    fontFamily: "Hanken Grotesk, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "1.5rem"
    fontWeight: 700
    lineHeight: 1
    letterSpacing: "-0.035em"
rounded:
  pill: "999px"
  window: "12px"
  keycap: "8px"
  focus: "6px"
  keycap-small: "5px"
spacing:
  gutter: "1rem"
  stack: "1.25rem"
  row: "1.3rem"
  column: "4rem"
  section: "clamp(3rem, 8vh, 5rem)"
  content-max: "84rem"
components:
  button-primary:
    backgroundColor: "{colors.hero-button}"
    textColor: "{colors.hero-button-ink}"
    rounded: "{rounded.pill}"
    padding: "0.85rem 1.4rem"
  button-primary-dark:
    backgroundColor: "{colors.hero-button-dark}"
    textColor: "{colors.hero-button-ink-dark}"
    rounded: "{rounded.pill}"
    padding: "0.85rem 1.4rem"
  segmented-option:
    textColor: "{colors.sky-ink}"
    typography: "{typography.label}"
    rounded: "{rounded.pill}"
    padding: "0.3rem 0.72rem"
  segmented-option-active:
    backgroundColor: "{colors.sky-ink}"
    textColor: "{colors.sky-top}"
    rounded: "{rounded.pill}"
  footer-switch-option-active:
    backgroundColor: "{colors.text}"
    textColor: "{colors.page}"
    rounded: "{rounded.pill}"
  keycap:
    backgroundColor: "{colors.keycap}"
    textColor: "{colors.text}"
    rounded: "{rounded.keycap}"
    height: "2.2rem"
    padding: "0 0.5rem"
  keycap-dark:
    backgroundColor: "{colors.keycap-dark}"
    textColor: "{colors.text-dark}"
    rounded: "{rounded.keycap}"
    height: "2.2rem"
  keycap-on-ridge:
    backgroundColor: "rgba(255,255,255,0.16)"
    textColor: "{colors.ridge-ink}"
    rounded: "{rounded.keycap-small}"
    height: "1.5rem"
  app-window-miniature:
    backgroundColor: "#fbfbfa"
    textColor: "#17171a"
    rounded: "{rounded.window}"
    height: "29.25rem"
  app-window-miniature-dark:
    backgroundColor: "#121214"
    textColor: "#ececee"
    rounded: "{rounded.window}"
---

# Design System: Jolta website

## Overview

**Creative North Star: "Sky by the Hour"**

The page is the sky that the Mac app shows at inbox zero. The same hues come from `apple/Jolta/Scenery.swift`, and the visitor's clock picks the phase: dawn (05:00 to 07:59), day (08:00 to 16:59), dusk (17:00 to 19:59) or night. The theme picks Graphite Light or Graphite Dark mixes of those hues. Three painted hill planes sit at the foot of the sky and move at different speeds as the page scrolls, like layers of a cel animation. The live miniature of the app window stands on the hills, and the near ridge passes in front of its lower edge.

Below the ridge, the hill dissolves into a flat ground, and the reading sections (keys, privacy, facts) sit on that ground with hairline rules and no cards. The page ends back in the sky: the closing band is one tone of the current sky with its own two ridges, then the footer sits on the ground again. Type is a quiet sans for reading and one heavy serif for the few headlines. Colour is carried by the sky and hills; the interface itself stays close to black and white, with one amber bolt and one orange focus ring.

The world rejects the category default for mail apps: a dark gradient hero with a glowing product screenshot.

**Key Characteristics:**
- The sky and hill colours are computed at runtime from the hour and theme, never fixed per section.
- Depth comes from stacked hill planes, not from shadows or cards.
- Gloock appears only at headline size; everything else is Hanken Grotesk.
- One amber bolt for the mark and the inbox-zero moment; orange marks keyboard focus.
- Light and dark themes follow the system, with a System, Light and Dark switch in the footer.
- Everything is served from jolta.app itself: no inline scripts, no inline style attributes, no third-party requests.

## Colors

A painted landscape palette that changes with the hour, over a near-monochrome interface.

### Primary
- **Daytime Sky Blue** (sky-top) to **Haze Blue** (sky-bottom): the top and horizon of the hero gradient, which runs from the top colour to the bottom colour at 78% of the height. These are the defaults the stylesheet declares; `site.js` replaces them with the current phase on load. The top colour is also written into the `theme-color` meta tag.
- **Meadow** (hill-far), **Pasture** (hill-mid) and **Shaded Ridge** (hill-near): the three hill planes, far to near. The near ridge carries the key hints and the "Try it here" line in Ridge White (ridge-ink).

### Secondary
- **Bolt Amber** (bolt-amber): the solid fill of the wordmark bolt and the text selection background (with `#1b1300` ink). The large inbox-zero bolt and the bolt in the miniature use a vertical gradient from Bolt Light through Bolt Amber to Bolt Deep, outlined in Bolt Edge.
- **Focus Orange** (focus, focus-dark): the 2px keyboard focus ring, offset 4px with a 6px radius. It is the same orange that marks keyboard focus in the Graphite app.

### Neutral
- **Hill Ground** (page, page-dark): the background below the sky. At runtime it is the near hill mixed with white or near-black (see The Ground Rule).
- **Ink** (text, text-dark) and **Soft Ink** (text-soft, text-soft-dark): headings and body; Soft Ink is for paragraphs, the lede, list descriptions and the footer.
- **Sky Ink** (sky-ink, sky-ink-soft): text on the sky. It is the phase's ink: dark navy by day in light theme, white (or `#ECECEE` in dark theme) otherwise.
- **Hairline** (rule, rule-dark): 1px rules between key items, fact rows, sections and the footer.
- **Keycap** (keycap, keycap-edge): keycaps on the ground.
- **Hero Button** (hero-button, hero-button-ink): the Download button, near-black on light skies and off-white on dark skies.

### The sky phases (from Scenery.swift)

Graphite Light uses the app's skies unchanged:

| Phase | Top | Bottom | Far hill | Near hill | Ink |
|---|---|---|---|---|---|
| Dawn | #5B6EA8 | #F2B48A | #6E6A8E | #48466A | #FFFFFF |
| Day | #5FA8E8 | #CFE8F7 | #8DBF8A | #5F9A63 | #17304A |
| Dusk | #2E3A78 | #F08A5D | #6B4F7A | #3B2E52 | #FFFFFF |
| Night | #0B1026 | #26305A | #1C2244 | #121630 | #FFFFFF |

Graphite Dark mixes Scenery's hues (blue #5FA8E8, cyan #8FD0F0, green #8DBF8A, magenta #8E6AA8, orange #F08A5D) a little into the app's background `#121214` and its deepest tone `#0C0C0E`, so the sky stays dark at every hour. Stars are white and show whenever the theme is dark or the phase is not day: half opacity, full opacity at night.

### Named Rules

**The Clock Sets the Sky Rule.** Sky, hill, ink and button colours come from `site.js`, which reads the Scenery hues, the visitor's hour and the theme. Never hand-pick a sky colour for a section; derive it from the current phase. The stylesheet values are only the first-paint defaults for a daytime sky.

**The Plane Step Rule.** In light theme the planes are the phase's far hill, the mix of far and near at 50%, and the near hill; by day the near hill is darkened 22% toward black so white key hints stay readable. In dark theme the far plane is the sky bottom mixed 32% toward the hill hue, the near plane is `#0C0C0E` mixed toward the hill hue at 45% of the phase's hill amount, and the mid plane is halfway between them, so each step is visible in the dark.

**The Ground Rule.** The ground below the ridge is the phase's near hill mixed 90% toward white in light theme, or 30% toward `#0C0C0E` in dark theme. The ridge fades into it over 6rem, so the sky and the reading sections are one landscape.

**The Earned Contrast Rule.** The closing band's colour is the lightest mix of the sky's top toward its bottom (starting at 65% and stepping back 5% at a time) on which white or `#17181A` reaches at least 4.8:1. Its text and button take whichever of the two wins. Its two ridges each stand at least 1.3:1 from what is behind them, and a glow halfway to the sky bottom sits below the text.

**The One Bolt Rule.** Amber belongs to the bolt: the wordmark, the inbox-zero strike and text selection. It is never a button, link or text colour.

## Typography

**Display Font:** Gloock (with New York, Georgia, serif)
**Body Font:** Hanken Grotesk (with -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif)

**Character:** A heavy, high-contrast serif for a handful of short headlines over a plain, open grotesk that does all the reading and interface work. Both are self-hosted WOFF2 files under the SIL Open Font License; the two upright files are preloaded.

### Hierarchy
- **Display** (Gloock 400, clamp(3rem, 6.4vw, 5.4rem), line height 0.98, -0.02em): the hero headline only, with balanced wrapping.
- **Headline** (Gloock 400, clamp(2.4rem, 5vw, 4.2rem), line height 1, -0.015em): one per section, including the closing band (max 14ch there). The legal pages' title uses clamp(2.6rem, 6vw, 4rem) at line height 1.02.
- **Title** (Hanken Grotesk 600, 1.1rem, line height 1.3): the name of each key item. Legal section headings use 600 at 1.3rem with -0.01em.
- **Body** (Hanken Grotesk 400, 1.0625rem, line height 1.6): all running text. The hero lede is 1.12rem with a 30rem measure; the privacy lede is 1.15rem with a 34rem measure; legal text is 1.05rem at line height 1.75 within 46rem.
- **Label** (Hanken Grotesk 500, 0.82rem, line height 1.2): segmented switch options. Small print (system line, footer, fact note) is 0.85 to 0.86rem in Soft Ink.
- **Wordmark** (Hanken Grotesk 700, 1.5rem, -0.035em, lowercase "jolta"): beside the bolt in the top bar.

Numbers in the facts list are bold Ink with tabular figures; the sentence around them is Soft Ink.

### Named Rules

**The Two Voices Rule.** Gloock is for the hero headline and section headlines, at 2.4rem or larger, weight 400. Every label, button, title, number and paragraph is Hanken Grotesk.

**The App Speaks in SF Rule.** The app miniature uses the system font stack (-apple-system, SF Pro Text) at the app's own pixel sizes (11 to 15px, 13px rows), never the site's fonts, because it shows the real app.

## Layout

Content sits in one container, `min(100% - 2rem, 84rem)`, centred, so the side gutter is 1rem on phones. The hero is at least the full small viewport height (100svh): the bar, then a two-column title row (7fr headline, 5fr lede, download button and system line, bottom-aligned), then the 58rem-wide app window, centred and set on the hills, with the key hints centred on the near ridge 1.6rem above the sky's bottom edge.

Reading sections use plain two-column grids: the six key items in two columns with a 4rem column gap and 1.3rem row padding, each item a 7.5rem keycap column beside its text; the privacy section with its argument on the left and the measured facts on the right (3rem by 5rem gap). Vertical rhythm uses clamps tied to viewport height, such as clamp(3rem, 8vh, 5rem) above a section and clamp(8rem, 14vh, 10rem) below the hero ridge.

Responsive changes: at 1200px and below, the inbox-zero bolt moves to the lower right of the hero. At 900px the title row, key grid and privacy grid become one column and the main links hide, leaving the wordmark and the hour switch. At 640px the app window drops its sidebar and its rows wrap to two lines. At 520px the key items stack their keycaps above their text and the footer switch moves to the start of its line.

## Elevation & Depth

Depth comes from the landscape: three hill planes at different heights, layered behind and in front of the app window, each moving with the scroll at its own speed (far 0.08, mid 0.16, near 0.3 of the scroll distance, capped at 1.5 viewport heights). Reading sections are flat, separated by hairlines, with no cards. Shadows are kept for the two objects that stand on the landscape: the Download button and the app window. At dusk and night a warm radial glow (rgba(255,196,80,0.28)) appears behind the window, like a lit window at evening. Reduced motion stops the parallax and all transitions.

### Shadow Vocabulary
- **Button lift** (`box-shadow: 0 8px 22px -10px rgba(0,0,0,.55)`): the Download button, on the sky and in the closing band.
- **Window float** (`box-shadow: 0 0 0 .5px var(--jw-edge), 0 24px 60px -12px rgba(10,10,20,.4), 0 6px 16px -6px rgba(10,10,20,.22)`): the app miniature only.
- **Pressed option** (`box-shadow: 0 1px 2px rgba(0,0,0,.18)`): the selected option of the hour switch on the sky. The footer switch drops it.
- **Bolt glow** (`filter: drop-shadow(0 0 40px rgba(255,200,60,.6))`): the large bolt that appears at inbox zero.

### Named Rules

**The Painted Planes Rule.** Depth is made by stacking landscape planes that move at different speeds, not by lifting panels with shadows. A new section that needs separation gets a hairline or a change of ground, not a card.

## Shapes

Two families. Controls on the page are fully rounded pills (999px): the Download button, the segmented switches and their options. Keys are small rounded squares (8px, 2.2rem tall, with a 3px bottom edge so they read as keycaps; 5px and 1.5rem on the ridge). Hills are soft, irregular curves drawn as SVG paths stretched to the full width, wider than the viewport by 4% so their ends never show. Stars are small circles in three sizes (1.5, 2.2 and 3.2px), placed by a seeded random scatter with a minimum gap. The app window has the macOS 12px corner.

## Components

### Buttons
- **Shape:** fully rounded pill (999px), 0.85rem by 1.4rem padding, a 1rem download arrow icon drawn as an inline SVG, Hanken Grotesk 600.
- **Primary:** Hero Button colours, which switch with the sky: near-black with white text on the daytime light sky, off-white with `#121214` text on darker skies. In the closing band the button takes the band's contrast-picked colours.
- **Hover / Focus:** rises 1px and brightens 8% over 0.25s with the house ease (cubic-bezier(.16,1,.3,1)); focus shows the Focus Orange ring.
- There is one button style. Links are underlined text (1px, 0.22em offset).

### Segmented switches
- **Style:** a pill track filled with the current text colour at 13%, 3px padding, options in the Label style.
- **Hour switch (top bar):** Dawn, Day, Dusk, Night. The selected option is Sky Ink on the sky's top colour, so it inverts with every phase. Picking an hour stops the page from following the clock.
- **Theme switch (footer, every page):** System, Light, Dark, stored in `localStorage`. The selected option is Ink with Hill Ground text.

### Keycaps
- **On the ground:** 2.2rem square minimum, 8px radius, Keycap fill with a 1px Keycap Edge border and a 3px bottom border, Hanken Grotesk 600 at 1rem. Two keys for a chord (⌘ K, J K) sit 0.3rem apart.
- **On the ridge:** 1.5rem, 5px radius, white at 16% with a 38% white border, inside Ridge White text with a soft text shadow.

### Lists and rows
- **Key items:** a hairline above each, keycaps on the left, a Title and a Soft Ink line on the right.
- **Facts:** a hairline above each row; the measured number is bold Ink with tabular figures, the rest Soft Ink, and a small note says how the numbers were measured.

### Navigation
- **Top bar:** bolt and lowercase wordmark on the left; Keyboard, Privacy and Source code links at 0.95rem and 88% opacity on the right (full opacity and underline on hover), then the hour switch. Text colour is Sky Ink. Below 900px the links hide.
- **Legal pages:** the same bar with only the mark, in Ink on the ground.
- **Footer:** a hairline above, company line, Privacy, Terms and conditions and Contact links, then the theme switch at the right edge.

### App window miniature (signature component)
A live, keyboard-driven copy of Jolta's main window that mirrors the app's Graphite look. Its colours are Graphite Light and Graphite Dark tokens scoped to the miniature (window `#fbfbfa` or `#121214`, sidebar `#f3f3f1` or `#0c0c0e`, focus fill `#eeeeeb` or `#202025`, accent `#e5521a` or `#ff6b2c`), and they are not part of the site's palette. It has the macOS traffic lights, a sidebar with mailboxes and accounts, a search field, eight invented messages and a footer of key hints. The current row has the focus fill, an orange triangle at its left edge (the app's cursor marker) and its own E, H and R keys in place of its time. J, K, E, H, Z and / work while the window is on screen. Archiving the last message shows the inbox-zero scene in the current sky, and the large amber bolt drops into the hero. Below 640px it drops the sidebar and wraps rows to two lines. It is 29.25rem tall on the homepage and hidden when scripts do not run.

### Sky band (signature component)
The hero and the closing band share one structure: a vertical sky gradient, a star layer, and SVG hill planes filled from the plane colours, all repainted together with a 0.9s transition when the hour or theme changes.

## Do's and Don'ts

### Do:
- **Do** take every sky, hill, ink and button colour from the phase and theme through `site.js`, using the Scenery.swift hues.
- **Do** end each sky in a ridge that fades into the ground (6rem in the hero, 3rem in the closing band).
- **Do** check contrast when a colour sits on the sky: 4.8:1 for text in the closing band, 1.3:1 between neighbouring ridges.
- **Do** mark keyboard focus with the 2px Focus Orange ring (focus, focus-dark), offset 4px.
- **Do** keep the app miniature in Graphite tokens and the system font, so it matches screenshots of the real app.
- **Do** self-host every font and script on jolta.app; set any runtime style through the style object (`element.style`), because the Content-Security-Policy blocks inline style attributes and inline scripts.
- **Do** let `prefers-reduced-motion` stop the parallax and transitions.

### Don't:
- **Don't** build a dark gradient hero with a glowing product screenshot.
- **Don't** set stars in rows or a grid; scatter them with the seeded random placement and a minimum gap.
- **Don't** use amber for buttons, links or text; it belongs to the bolt.
- **Don't** put Gloock below headline size or Hanken Grotesk in the app miniature.
- **Don't** wrap reading sections in cards or shadows; use hairlines and the ground.
- **Don't** load fonts, scripts, images or analytics from another domain.
