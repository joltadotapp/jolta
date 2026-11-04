# Jolta

A native, keyboard-first, local-first email client, called Kite (its codename) until 2026-11-03. A Rust core holds all mail logic; a SwiftUI + AppKit app for macOS 26+ is the first UI.

If `AGENTS.local.md` exists, read it first: it describes this checkout's machines and accounts, and the planning docs kept outside the repository. `docs/architecture.md` describes the shape of the code, the data model and the bridge.

## How to work here

- Functionality first. Write a test where logic can break silently. No CI, fuzzing, lint configs or benchmark tracking yet.
- Keep tooling minimal: cargo, the scripts in `scripts/`, a hand-made Xcode project. Ask before adding a tool or a build step.
- Every dependency must be MIT, Apache-2.0, BSD, ISC, Zlib or MPL-2.0. No GPL, AGPL or LGPL code, copied or linked.
- Jolta works fully without any server of ours. An optional subscription service may come later for features that need one; it adds to the app and never gates the core. No telemetry containing mail data.
- Mail and calendar logic stays in Rust. Swift renders and forwards user intents.
- The repository is public. The hooks in `.githooks/` refuse secrets (gitleaks), anything listed in `.git/info/private-strings` in changes or commit messages, and any commit email that isn't a GitHub no-reply address. Turn them on in a new clone with `git config core.hooksPath .githooks` and `brew install gitleaks`. Refer to people and test accounts by role, never by name or address.
- Verify on the real artifact: the `jolta` CLI against a real account, or the app itself.

## Build

```sh
scripts/build-core.sh
xcodebuild -project apple/Jolta.xcodeproj -scheme Jolta -configuration Debug build
```

Build the `jolta` CLI with `scripts/build-cli.sh`, never plain `cargo build`: the script signs it with the Apple Development certificate. An unsigned `jolta` has a new identity after every build, so its first Keychain read opens a password dialog, and on a Mac without anyone at the screen the command hangs.

Release builds are Apple silicon only; `JOLTA_INTEL=1` in front of `scripts/build-core.sh --release` or `scripts/release.sh` builds the universal app. Debug builds don't run Sparkle.

Rust toolchain and dependency versions are pinned in `rust-toolchain.toml` and the workspace `Cargo.toml`. UniFFI is pinned to `=0.31.2`; don't bump it without checking `uniffi-bindgen-cs` support.
