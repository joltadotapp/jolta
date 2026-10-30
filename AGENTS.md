# Kite (codename)

A native, keyboard-first, local-first email client. A Rust core holds all mail logic; a SwiftUI + AppKit app for macOS 26+ is the first UI.

Read these before changing anything:

- `docs/decisions.md`: the locked product and stack decisions. They override the original handoff. Don't reopen them without a new fact; if one turns out wrong, say so and update the file.
- `docs/architecture.md`: the shape of the code, the data model and the bridge.
- `docs/mvp-plan.md`: milestones, their done checks, and what only Koen can provide.
- `docs/research/`: the evidence behind the decisions, with sources.

## How to work here

- Functionality first. Write a test only where the plan names one or where logic can break silently. No CI, fuzzing, lint configs or benchmark tracking until the MVP works.
- Keep tooling minimal: cargo, one build script, a hand-made Xcode project. Ask before adding a tool or a build step.
- Every dependency must be MIT, Apache-2.0, BSD, ISC, Zlib or MPL-2.0. No GPL, AGPL or LGPL code, copied or linked.
- Gmail data never passes through a server we run. No relays, no hosted services, no telemetry containing mail data.
- Mail logic stays in Rust. Swift renders and forwards user intents.
- Verify each milestone on the real artifact: the `kite` CLI against a real account, or the app running on the Mac mini.

## Machines

- Day-to-day work runs on the Mac mini (macOS 27, Xcode 27.1, Rust 1.99.1). Build and run the app there.
- The MacBook Pro runs macOS 27.2 and has Kite installed from the update feed in `/Applications`. Reach it over SSH (`ssh macbook`) and launch the app with `open -g` so no window comes to the front while Koen works.
- No machine runs macOS 26, the minimum target, any more. It gets checked in a macOS 26 VM at the end of the MVP.
- Debug builds don't run Sparkle; release builds come only from `scripts/release.sh <version>`, one publish per version.
- Agents may send test mail only to Koen's own addresses.

## Build

```sh
scripts/build-core.sh
xcodebuild -project apple/Kite.xcodeproj -scheme Kite -configuration Debug build
```

Build the `kite` CLI with `scripts/build-cli.sh`, never plain `cargo build`: the script signs it with the Apple Development certificate. An unsigned `kite` has a new identity after every build, so its first Keychain read opens a password dialog on the Mac mini's screen, which is headless, and the command hangs.

Rust toolchain and dependency versions are pinned in `rust-toolchain.toml` and the workspace `Cargo.toml`. UniFFI is pinned to `=0.31.2`; don't bump it without checking `uniffi-bindgen-cs` support.
