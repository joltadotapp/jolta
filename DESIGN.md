---
name: Jolta website
description: The jolta.app site. The app's inbox-zero sky, set by the visitor's clock and theme, is the page.
colors:
  sky-top: "#5FA8E8"
  sky-bottom: "#CFE8F7"
  hill-back: "#aed4c1"
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
  accent-text: "#b23a0a"
  keycap-low: "#f3f4f0"
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
  accent-text-dark: "#ff8a57"
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

The page is the sky that the Mac app shows at inbox zero. The same hues come from `apple/Jolta/Scenery.swift`, and the visitor's clock picks the phase: dawn (05:00 to 07:59), day (08:00 to 16:59), dusk (17:00 to 19:59) or night. The theme picks Graphite Light or Graphite Dark mixes of those hues. Four painted hill planes sit at the foot of the sky and move at different speeds as the page scrolls, like layers of a cel animation. Each plane fades toward the sky's colour at its foot, so the crest in front of it stands out. Trees, a house and a letterbox stand on the ridges as flat silhouettes. The sun or the moon marks the hour: low on the skyline at dawn and dusk, high by day and at night. The live miniature of the app window stands on the hills, and the near ridge passes in front of its lower edge.

Below the ridge, the hill dissolves into a flat ground, and the reading sections (keys, the route diagram, facts) sit on that ground with hairline rules and no cards. The page ends back in the sky: the closing band is one tone of the current sky with its own two ridges, then the footer sits on the ground again. Type is a quiet sans for reading and one heavy serif for the few headlines. Colour is carried by the sky and hills; the interface itself stays close to black and white, with one amber bolt and one orange focus ring.

The world rejects the category default for mail apps: a dark gradient hero with a glowing product screenshot.

**Key Characteristics:**
- The sky and hill colours are computed at runtime from the hour and theme, never fixed per section.
- Depth comes from stacked hill planes, not from shadows or cards.
- The page answers the keyboard: the demo window, the list of keys and the feature tabs all react to the keys they describe.
- Gloock appears only at headline size; everything else is Hanken Grotesk.
- One amber bolt for the mark and the inbox-zero moment; orange marks keyboard focus.
- Light and dark themes follow the system, with a System, Light and Dark switch in the footer.
- Everything is served from jolta.app itself: no inline scripts, no inline style attributes, no third-party requests.

## Colors

A painted landscape palette that changes with the hour, over a near-monochrome interface.

### Primary
- **Daytime Sky Blue** (sky-top) to **Haze Blue** (sky-bottom): the top and horizon of the hero gradient, which runs from the top colour to the bottom colour at 78% of the height. These are the defaults the stylesheet declares; `site.js` replaces them with the current phase on load. The top colour is also written into the `theme-color` meta tag.
- **Haze** (hill-back), **Meadow** (hill-far), **Pasture** (hill-mid) and **Shaded Ridge** (hill-near): the four hill planes, far to near. The near ridge carries the key hints and the "Try it here" line in Ridge White (ridge-ink).

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

**The Plane Step Rule.** In light theme the planes are the phase's far hill, the mix of far and near at 50%, and the near hill; by day the near hill is darkened 22% toward black so white key hints stay readable. The planes always run lightest to darkest from far to near, each at least 1.3:1 from the one behind it: when the far hill sits too close to the sky (Graphite Light's night), it lifts into the haze above the sky's bottom and the mid and near planes step down from it, never reaching black. In dark theme the far plane is the sky bottom mixed 32% toward the hill hue, the near plane is `#0C0C0E` mixed toward the hill hue at 45% of the phase's hill amount, and the mid plane is halfway between them, so each step is visible in the dark.

**The Haze Rule.** The farthest range is the far plane mixed 50% toward the sky's bottom colour. The back, far and mid planes each run from their own colour at the crest to a mix toward the sky's bottom at their foot (45%, 30% and 20%). The near ridge stays one flat colour, because the ground fades out of it.

**The Sky's Own Light Rule.** The sun and the moon are the sky's bottom colour mixed toward white: 82% in light theme, 42% in dark theme by day and 72% at night, so the sun never competes with the amber bolt. Clouds are the sky's bottom mixed 80% toward white on a light daytime sky, a mix of the sky's bottom and top at other hours, and a faint lift of the sky in dark theme.

**The Ground Rule.** The ground below the ridge is the phase's near hill mixed 90% toward white in light theme, or 30% toward `#0C0C0E` in dark theme. The ridge fades into it over 6rem, or 2.5rem under a near-black ridge in light theme so the fade never passes through grey, so the sky and the reading sections are one landscape.

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

Reading sections use plain grids. The six key items sit in two columns that fill top to bottom (4rem column gap, 1.4rem row padding), each item a 7.5rem keycap column beside its text, so J and K step down one column and on to the next. The privacy section has its headline on the left and its argument on the right, then the route diagram across the full width, then the three measured facts in three columns. Vertical rhythm uses clamps tied to viewport height, such as clamp(3rem, 8vh, 5rem) above a section and clamp(8rem, 14vh, 10rem) below the hero ridge.

Responsive changes: at 1200px and below, the inbox-zero bolt moves to the lower right of the hero. At 900px the title row, key grid, privacy heading and facts become one column, the clouds hide, and the main links hide, leaving the wordmark and the hour switch (the hero's "See every feature" link and the footer still lead to the features page). At 760px the route diagram shortens its labels and the postmark moves above the closing headline. At 640px the app window drops its sidebar and its rows wrap to two lines. At 520px the key items stack their keycaps above their text and the footer switch moves to the start of its line.

## Elevation & Depth

Depth comes from the landscape: three hill planes at different heights, layered behind and in front of the app window, each sinking as the page scrolls at its own share of the scroll distance (sun, moon and clouds 0.4, back 0.32, far 0.25, mid 0.12, near 0), capped at 1.5 viewport heights. Reading sections are flat, separated by hairlines, with no cards. Shadows are kept for the two objects that stand on the landscape: the Download button and the app window. At dusk and night a warm radial glow (rgba(255,196,80,0.28)) appears behind the window, like a lit window at evening. Reduced motion stops the parallax, the drifting clouds, the twinkling stars, the envelopes on the route and all transitions.

### Shadow Vocabulary
- **Button lift** (`box-shadow: 0 8px 22px -10px rgba(0,0,0,.55)`): the Download button, on the sky and in the closing band.
- **Window float** (`box-shadow: 0 0 0 .5px var(--jw-edge), 0 24px 60px -12px rgba(10,10,20,.4), 0 6px 16px -6px rgba(10,10,20,.22)`): the app miniature only.
- **Pressed option** (`box-shadow: 0 1px 2px rgba(0,0,0,.18)`): the selected option of the hour switch on the sky. The footer switch drops it.
- **Bolt glow** (`filter: drop-shadow(0 0 40px rgba(255,200,60,.6))`): the large bolt that appears at inbox zero.

### Named Rules

**The Painted Planes Rule.** Depth is made by stacking landscape planes that move at different speeds, not by lifting panels with shadows. A new section that needs separation gets a hairline or a change of ground, not a card.

## Shapes

Two families. Controls on the page are fully rounded pills (999px): the Download button, the segmented switches and their options. Keys are small rounded squares (8px, 2.2rem tall, with a 3px bottom edge so they read as keycaps; 5px and 1.5rem on the ridge). Hills are soft, irregular curves drawn as SVG paths stretched to the full width, wider than the viewport by 4% so their ends never show. Trees (pine, round crown, cypress), the house and the letterbox are flat silhouettes in the colour of the plane they stand on; `site.js` scales each one back to its own proportions and sets it on the crest, because the hills stretch with the page. Clouds are stacks of two or three pills. The moon is a crescent; the sun is a plain disc with a soft glow of its own colour. Stars are small circles in three sizes (1.5, 2.2 and 3.2px), placed by a seeded random scatter with a minimum gap. The app window has the macOS 12px corner.

## Components

### Buttons
- **Shape:** fully rounded pill (999px), 0.85rem by 1.4rem padding, a 1rem download arrow icon drawn as an inline SVG, Hanken Grotesk 600.
- **Primary:** Hero Button colours, which switch with the sky: near-black with white text on the daytime light sky, off-white with `#121214` text on darker skies. In the closing band the button takes the band's contrast-picked colours.
- **Hover / Focus:** rises 1px and brightens 8% over 0.25s with the house ease (cubic-bezier(.16,1,.3,1)); focus shows the Focus Orange ring.
- There is one button style. Links are underlined text (1px, 0.22em offset).

### Segmented switches
- **Style:** a pill track filled with the current text colour at 13%, 3px padding, options of equal width in the Label style. Where the browser supports `:has()`, one thumb slides between the options over 0.5s; elsewhere the selected option carries the fill itself.
- **Hour switch (top bar):** Dawn, Day, Dusk, Night. The selected option is Sky Ink on the sky's top colour, so it inverts with every phase. Picking an hour stops the page from following the clock.
- **Theme switch (footer, every page):** System, Light, Dark, stored in `localStorage`. The selected option is Ink with Hill Ground text.

### Keycaps
- **On the ground:** 2.5rem square minimum, 9px radius, a top face that runs from Keycap to Keycap Low, a 1px Keycap Edge border and a 2px wall below it in the same colour, Hanken Grotesk 600 at 1.05rem. A pressed key moves down 2px and loses its wall. Two keys for a chord (⌘ K, J K) sit 0.35rem apart. Tabs and key lists on the features page use the same keycap at 2.1rem and 1.9rem.
- **Pressed from the keyboard:** on the homepage, a key the visitor presses goes down on the page until they let go.
- **On the ridge:** 1.5rem, 5px radius, white at 16% with a 38% white border, inside Ridge White text with a soft text shadow.

### Lists and rows
- **Key items:** a hairline above each, keycaps on the left, a Title and a Soft Ink line on the right. One item is current, marked as in the app: an orange triangle at its left edge and its keycaps in the accent text colour. J and K move the mark, an item's own key jumps to it, and so does the pointer. The demo window gets the keys first while it is on screen.
- **Facts:** three columns with a hairline above each; the measured number is 1.45rem bold Ink with tabular figures on its own line, the rest of the sentence Soft Ink, and a small note says how the numbers were measured.

### Route diagram (signature component)
The picture of "nobody in between". Four providers on the left (Gmail, Microsoft 365, iCloud, IMAP), each with a ring and a 1.5px line that runs straight to a bracket and on to an outlined Mac with the bolt on its screen. A dashed, empty box labelled "No Jolta server" sits over the middle of the lines, and they pass through it. Small envelopes travel each line from provider to Mac while the diagram is on screen; with reduced motion they stand still along the lines. It is built from HTML and CSS, in Ink on the ground, with no fill colour besides the bolt.

### Postmark
A round postmark with wavy cancellation lines sits beside the closing headline, turned 9 degrees, in the band's text colour at 42%. It shows the visitor's own time, date and year, like the postmark on the app icon's postcard, and updates every minute.

### Navigation
- **Top bar:** bolt and lowercase wordmark on the left; Features, Privacy and Source code links (the current page's link is underlined at 2px) at 0.95rem and 88% opacity on the right (full opacity and underline on hover), then the hour switch. Text colour is Sky Ink. Below 900px the links hide.
- **Legal pages:** the same bar with only the mark, in Ink on the ground.
- **Footer:** a hairline above, company line, Privacy, Terms and conditions and Contact links, then the theme switch at the right edge.

### App window miniature (signature component)
A live, keyboard-driven copy of Jolta's main window that mirrors the app's Graphite look. Its colours are Graphite Light and Graphite Dark tokens scoped to the miniature (window `#fbfbfa` or `#121214`, sidebar `#f3f3f1` or `#0c0c0e`, focus fill `#eeeeeb` or `#202025`, accent `#e5521a` or `#ff6b2c`), and they are not part of the site's palette. It has the macOS traffic lights, a sidebar in the app's order (Inbox, Starred, Mailboxes, Next to wake, Accounts), a search field, eight invented messages and a footer of key hints. The current row has the focus fill, an orange triangle at its left edge (the app's cursor marker) and its own E, H and R keys in place of its time. J, K, E, H, Z and / work while the window is on screen. H snoozes the current message until 09:00 tomorrow and lists it under Next to wake as in the app: a monospaced short weekday and time (10.5px, 58px column) and the subject, at most five rows, with the Accounts group folding to its unread count once two or more are snoozed. After an action the message that left sweeps out to the right in 170ms and the rows below close the gap in 240ms. The key hints in the footer and the E and H on the current row are buttons, so the demo also works by touch, and the hint for a pressed key lights up for a moment. Archiving the last message shows the inbox-zero scene in the current sky: the sky flashes once, the large amber bolt drops into the hero on the side the sun or the moon leaves free, and the flag on the letterbox drops. Undo takes all three back. Below 640px it drops the sidebar and wraps rows to two lines. It is 29.25rem tall on the homepage and hidden when scripts do not run.

### Feature stage (features page)
One stage plays a recording of the real app for one feature at a time. Above it, keycap tabs (E Triage, ↩ Read, / Search, ⌘K Commands, H Snooze, C Write) follow the WAI tabs pattern, and the same keys on the visitor's keyboard switch the clip. The clip is a muted, looping H.264 MP4 of the demo app in Graphite Light or Dark, matched to the page theme, clipped to the window's corner (2.1% / 3.33%) with the Window float shadow family. It plays only while on screen; a Pause and Play pill (dark glass, bottom right) is always present, and with reduced motion the clip waits behind a Play button. Below 640px the clip switches to a crop around the pane the feature acts on (`-phone` files), at the crop's own aspect ratio; the palette crops fade out at their right edge. Each clip has a WebP still taken at the moment the feature is on screen. A text panel beside the stage (below it on phones) gives a Headline, a Soft Ink paragraph and the clip's keys as keycaps. Recordings come from the jolta-demo skill at the compact window size; the Worker serves `/media/` with byte ranges because Safari needs them.

### Sky band (signature component)
The hero and the closing band share one structure: a vertical sky gradient, a star layer, and SVG hill planes filled from the plane colours, all repainted together with a 0.9s transition when the hour or theme changes. Every fifth star twinkles at its own pace, and at night a shooting star crosses the hero every 6 to 15 seconds.

The hero also has the sun, the moon and four clouds. Each hour lists the places the sun or the moon prefers; `site.js` measures the lines of text, the hills and the app window and takes the first place that is clear, or leaves the sky empty when none is. When the hour changes, the sun or the moon travels there in an arc over 1.5s, and the one that is not in the sky sets behind the skyline. On a phone the sun sets behind the app window. Clouds drift left to right in the band between the title and the hills (one crossing takes 210 to 320 seconds) and are hidden below 900px and on the features page.

On load the hill planes rise 3rem into place, the title and the window settle, and the sun or the moon comes up. This is the page's one entrance; sections below do not animate in.

## Do's and Don'ts

### Do:
- **Do** take every sky, hill, ink and button colour from the phase and theme through `site.js`, using the Scenery.swift hues.
- **Do** end each sky in a ridge that fades into the ground (6rem in the hero, 3rem in the closing band).
- **Do** check contrast when a colour sits on the sky: 4.8:1 for text in the closing band, 1.3:1 between neighbouring ridges.
- **Do** mark keyboard focus with the 2px Focus Orange ring (focus, focus-dark), offset 4px.
- **Do** keep the app miniature in Graphite tokens and the system font, so it matches screenshots of the real app.
- **Do** self-host every font and script on jolta.app; set any runtime style through the style object (`element.style`), because the Content-Security-Policy blocks inline style attributes and inline scripts.
- **Do** let `prefers-reduced-motion` stop the parallax and transitions, and hold recordings behind a Play button.
- **Do** keep the sun, the moon and the clouds clear of text: place them by measuring, never by a fixed position.
- **Do** draw anything new in the landscape as a flat silhouette in its plane's colour.
- **Do** show the real app in recordings of the demo copy on invented mail, and give every looping recording a visible pause control.

### Don't:
- **Don't** build a dark gradient hero with a glowing product screenshot.
- **Don't** set stars in rows or a grid; scatter them with the seeded random placement and a minimum gap.
- **Don't** use amber for buttons, links or text; it belongs to the bolt.
- **Don't** put Gloock below headline size or Hanken Grotesk in the app miniature.
- **Don't** wrap reading sections in cards or shadows; use hairlines and the ground.
- **Don't** load fonts, scripts, images or analytics from another domain.
- **Don't** colour the sun yellow or amber, or add shading, outlines or texture to the landscape.
- **Don't** animate reading sections into view on scroll; the hero's load is the only entrance.
