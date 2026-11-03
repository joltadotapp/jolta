# Jolta

A native, keyboard-first, local-first email client, called Kite (its codename) until 2026-11-03. A Rust core holds all mail logic; a SwiftUI + AppKit app for macOS 26+ is the first UI.

Read these before changing anything:

- `docs/decisions.md`: the locked product and stack decisions. They override the original handoff. Don't reopen them without a new fact; if one turns out wrong, say so and update the file.
- `docs/architecture.md`: the shape of the code, the data model and the bridge.
- `docs/mvp-plan.md`: milestones, their done checks, and what only Koen can provide.
- `docs/research/`: the evidence behind the decisions, with sources.

## How to work here

- Functionality first. Write a test only where the plan names one or where logic can break silently. No CI, fuzzing, lint configs or benchmark tracking until the MVP works.
- Keep tooling minimal: cargo, one build script, a hand-made Xcode project. Ask before adding a tool or a build step.
- Every dependency must be MIT, Apache-2.0, BSD, ISC, Zlib or MPL-2.0. No GPL, AGPL or LGPL code, copied or linked.
- Jolta works fully without any server of ours. An optional subscription service may come later for features that need one, such as shared conversations or sending while the Mac is off; it adds to the app and never gates the core. No telemetry containing mail data. Once Gmail data reaches a server we run, Google's security assessment (CASA) applies; see `docs/decisions.md`.
- Mail logic stays in Rust. Swift renders and forwards user intents.
- The repository is public. A pre-commit hook in `.githooks/` refuses secrets (gitleaks), personal addresses and host names listed in `.git/info/private-strings`, and any commit email that isn't a GitHub no-reply address. Turn it on in a new clone with `git config core.hooksPath .githooks` and `brew install gitleaks`. Use the test accounts' roles in docs (the Google Workspace test account), never their addresses; the update feed's address lives in `~/jolta-devid/feed-url`.
- Verify each milestone on the real artifact: the `jolta` CLI against a real account, or the app running on the Mac mini.

## Machines

- Day-to-day work runs on the Mac mini (macOS 27, Xcode 27.1, Rust 1.99.1). Build and run the app there.
- The MacBook Pro runs macOS 27.2 and has Jolta installed from the update feed in `/Applications`. Reach it over SSH (`ssh macbook`) and launch the app with `open -g` so no window comes to the front while Koen works.
- No machine runs macOS 26, the minimum target, any more. It gets checked in a macOS 26 VM when Koen asks for it, not while we are still iterating.
- Debug builds don't run Sparkle; release builds come only from `scripts/release.sh <version>`, one publish per version.
- Agents may send test mail only to Koen's own addresses.

## Build

```sh
scripts/build-core.sh
xcodebuild -project apple/Jolta.xcodeproj -scheme Jolta -configuration Debug build
```

Build the `jolta` CLI with `scripts/build-cli.sh`, never plain `cargo build`: the script signs it with the Apple Development certificate. An unsigned `jolta` has a new identity after every build, so its first Keychain read opens a password dialog on the Mac mini's screen, which is headless, and the command hangs.

Rust toolchain and dependency versions are pinned in `rust-toolchain.toml` and the workspace `Cargo.toml`. UniFFI is pinned to `=0.31.2`; don't bump it without checking `uniffi-bindgen-cs` support.
