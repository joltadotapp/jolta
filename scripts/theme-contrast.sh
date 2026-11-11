#!/usr/bin/env bash
# Checks that every theme's text, marks, fills and lines have enough contrast
# on every surface the app draws them on. It compiles the theme files with the
# checks in theme-contrast.swift and runs the result.

set -euo pipefail

cd "$(dirname "$0")/.."
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT
cp scripts/theme-contrast.swift "$OUT/main.swift"
swiftc -O -o "$OUT/check" apple/Jolta/ThemePalette.swift apple/Jolta/OmarchyThemes.swift apple/Jolta/Scenery.swift "$OUT/main.swift"
"$OUT/check"
