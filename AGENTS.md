# Jolta

A native, keyboard-first, local-first email client, called Kite (its codename) until 2026-11-03. A Rust core holds all mail logic; a SwiftUI + AppKit app for macOS 15.4+ is the first UI.

If `AGENTS.local.md` exists, read it first: it describes this checkout's machines and accounts, and the planning docs kept outside the repository. `docs/architecture.md` describes the shape of the code, the data model and the bridge.

## How to work here

- Functionality first. Write a test where logic can break silently. No CI, fuzzing, lint configs or benchmark tracking yet.
- Keep tooling minimal: cargo, the scripts in `scripts/`, a hand-made Xcode project. Ask before adding a tool or a build step.
- Every dependency must be MIT, Apache-2.0, BSD, ISC, Zlib or MPL-2.0. No GPL, AGPL or LGPL code, copied or linked.
- Jolta works fully without any server of ours. An optional subscription service may come later for features that need one; it adds to the app and never gates the core. No telemetry containing mail data.
- Mail and calendar logic stays in Rust. Swift renders and forwards user intents.
- The repository is public. The hooks in `.githooks/` refuse secrets (gitleaks), anything listed in `.git/info/private-strings` in changes or commit messages, and any commit email that isn't a GitHub no-reply address. Turn them on in a new clone with `git config core.hooksPath .githooks` and `brew install gitleaks`. Refer to people and test accounts by role, never by name or address.
- Verify on the real artifact: the app against a real account, or the `jolta` CLI against a data folder it signed in itself.

## Build

```sh
scripts/build-core.sh
xcodebuild -project apple/Jolta.xcodeproj -scheme Jolta -configuration Debug build
```

Build the `jolta` CLI with `scripts/build-cli.sh`, never plain `cargo build`: the script signs it with the Apple Development certificate. An unsigned `jolta` has a new identity after every build, so its first Keychain read opens a password dialog, and on a Mac without anyone at the screen the command hangs.

A sign-in made by the released app opens without a prompt only in a binary signed with the Developer ID certificate. To check such an account from a Debug build, sign the app with `codesign -f -s "Developer ID Application" <path>/Jolta.app`. The CLI still prompts after that.

Release builds are Apple silicon only; `JOLTA_INTEL=1` in front of `scripts/build-core.sh --release` or `scripts/release.sh` builds the universal app. Debug builds don't run Sparkle.

Rust toolchain and dependency versions are pinned in `rust-toolchain.toml` and the workspace `Cargo.toml`. UniFFI is pinned to `=0.31.2`; don't bump it without checking `uniffi-bindgen-cs` support.

## Test

- `scripts/test-servers.sh up`, then `JOLTA_SYNC_TESTS=1 cargo test --workspace`, runs the core tests and the sync tests against Dovecot and Mailpit in Docker. Without the variable, the sync tests skip silently when the servers are down.
- `scripts/ui-tests.sh [test names]` runs the app's UI journeys. It fills each test's data folder first; plain `xcodebuild test` skips every journey and still reports success.

## Changes

- Commit a failing test before its fix, so history shows red, then green.
- A commit message is one or two plain sentences saying what the user now gets.
- Migrations are numbered files (`crates/core/src/store/schemaN.sql`, listed in `MIGRATIONS`). Two branches can take the same number; the one that merges second renumbers.
- Update `docs/architecture.md` in the same branch when the data model, the bridge or a flow it describes changes.

## Website

The website, jolta.app, is a Cloudflare Worker in `site/`; `site/README.md` says how to run and deploy it. It also serves the public update feed, so its `/updates/` and `/download/` paths stay.
