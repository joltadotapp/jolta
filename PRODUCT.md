# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Mac mail power users: people who live in their inbox on Superhuman, Spark, Mimestream or Apple Mail and want speed and keyboard control. They judge a mail client by how fast it feels in the first minute and whether their hands can stay on the keyboard. The website's job is to win them over and get Jolta onto their Mac.

## Product Purpose

Jolta is a native, keyboard-first, local-first email and calendar client. A Rust core holds all mail and calendar logic; the Mac app is SwiftUI and AppKit. Success for the website is a visitor who downloads Jolta, connects an account, and keeps using it.

## Positioning

Fast, with no middleman. Jolta gives Superhuman-level speed, but the Mac connects straight to Google, Microsoft, iCloud and IMAP servers. No Jolta server stores, relays or reads mail, because there is no Jolta server in the path. Superhuman, Spark and other fast clients route mail or tokens through their own servers; Jolta cannot be copied on this point without giving up a server.

## Operating Context

- Accounts: Gmail and Google Workspace (IMAP with Gmail extensions), Microsoft 365 work or school (Microsoft Graph), iCloud, Fastmail and any IMAP server.
- Daily use is keyboard-driven: J and K to move, E to archive, H to snooze, R to reply, ⌘K command palette, / to search, Tab between split-inbox sections.
- Calendar runs in the same app and syncs itself (Google Calendar, Microsoft Graph, CalDAV, webcal feeds), off per account until the user turns it on.
- Distribution is a direct download from jolta.app with automatic updates (Sparkle). The site also serves the update feed and the download, so `/updates/` and `/download/` must keep working.

## Capabilities and Constraints

- Features: unified inbox, split inbox, search with operators, compose and send, undo send, scheduled send, snooze, follow-up reminders, snippets, ⌘K palette, keyboard shortcuts, new-mail notifications with an Important filter, calendar.
- AI is off until the user picks a provider: a model that runs on the Mac (Gemma 4 E2B, downloaded on demand), or the user's own OpenAI or Anthropic key, sent from the Mac straight to that provider. Features: conversation summaries, Write with AI (⌘J), optional sorting of new mail.
- MCP server: AI agents such as Claude Code can read, search, label, archive, snooze and draft mail through Jolta. Sending always needs approval in the app, and every agent action is logged.
- Platforms: macOS 15.4 or later now, Apple silicon only for public builds. Windows and Linux are planned after the Mac MVP; their UI technology is not decided yet.
- Price: free. An optional subscription service may come later for features that need a server; it never gates the core app. No pricing exists to show.
- Gmail limit: until Google verifies the app, at most 100 Gmail and Google Workspace users can sign in and they see Google's unverified-app screen. Microsoft 365, iCloud and IMAP have no such limit.
- No telemetry containing mail data.

## Brand Commitments

- Name: Jolta, lowercase wordmark "jolta" on the current site. Domain jolta.app.
- App icon (`apple/Jolta/Assets.xcassets/AppIcon.appiconset`): a cream postcard with a landscape stamp and postmark, an amber lightning bolt, and an orange calendar.
- The empty inbox in the app shows a sky with two hills that follows the time of day, and the amber bolt (`apple/Jolta/Scenery.swift`, `InboxZero.swift`).
- Company: Layer7 BV, Belgium. The footer carries the company name and VAT number.
- License: MIT. Source code is public at github.com/joltadotapp/jolta.
- Pages that must stay at their addresses: `/privacy` and `/terms-and-conditions` (named in the Google and Microsoft app registrations) and `/.well-known/microsoft-identity-association.json`.
- The Mac app's own look is Graphite: monochrome with one orange accent that marks keyboard focus (`docs/design/direction-a.html`, local only). The website may differ from it, but screenshots of the app will show Graphite.

## Evidence on Hand

- Measured performance on a Mac mini, release build: launch to first rows 232 to 367 ms; opening a conversation 11 to 85 ms; search first page about 1 ms on a real 84,000-message store; memory 64 to 86 MB with a conversation open.
- Reader privacy check: a hostile test message with 25 remote resources made 0 network requests (`scripts/leak-check.sh`).
- Real app screenshots and videos on invented mail can be made with the jolta-demo skill.
- Public source code and release history on GitHub.
- None of these exist and must not be invented: testimonials, user counts, customer logos, press quotes, ratings, pricing tiers, comparisons with measured competitor numbers.

## Product Principles

- Speed is felt, not claimed: show the app responding, and quote only measured numbers with how they were measured.
- No middleman: every claim about privacy follows from the mechanism, the Mac talking straight to the provider.
- The keyboard is the interface: the site shows which keys do what.
- The core never depends on a service of ours; anything paid adds to the app.
- AI is opt-in and runs where the user chooses.
