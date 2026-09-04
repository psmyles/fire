#!/usr/bin/env bash
# Wrap the built binary in a minimal .app bundle so Fire can be run from Finder / the Dock like a
# real macOS app, and optionally launch it.
#
# This is the *development* bundle, not the shipping one. `scripts/build-mac.sh` (Phase 2 step 9)
# is what produces the real thing: the .icns, the document types that make Finder open images with
# Fire, `codesign --options runtime`, notarization and the .dmg. This script exists because steps
# 5, 6 and 10 all need a bundled app to test against long before any of that is written — a bare
# executable gets no Dock presence, no proper activation, and is not what `open` hands file
# arguments to.
#
#   scripts/dev-app.sh                    build release, bundle, print the path
#   scripts/dev-app.sh some/image.png     ... and launch it on that image
#   scripts/dev-app.sh --debug img.png    bundle the debug build instead (faster iteration)
#   scripts/dev-app.sh --no-build         re-bundle whatever is already built
#
# Known gaps at this stage, so they are not mistaken for bugs: a generic icon, and no
# `CFBundleDocumentTypes`, so Finder will not route a double-clicked image here — `open -a` and a
# drop on the Dock icon do work, because those name the app explicitly.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile=release
build=1
args=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)    profile=debug; shift ;;
        --release)  profile=release; shift ;;
        --no-build) build=0; shift ;;
        -h|--help)  sed -n '2,19p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)          args+=("$1"); shift ;;
    esac
done

# Product metadata comes from product.json, the same single source build.rs reads, so the bundle
# cannot drift from the binary it wraps. plutil reads JSON, so this needs nothing installed.
name="$(plutil -extract productName raw -o - "$repo/product.json")"
version="$(plutil -extract version raw -o - "$repo/product.json")"

if [[ $build -eq 1 ]]; then
    if [[ $profile == release ]]; then
        cargo build --manifest-path "$repo/Cargo.toml" -p fire --release
    else
        cargo build --manifest-path "$repo/Cargo.toml" -p fire
    fi
fi

bin="$repo/target/$profile/fire"
[[ -x "$bin" ]] || { echo "no $profile binary at $bin — drop --no-build?" >&2; exit 1; }

app="$repo/target/$name.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$bin" "$app/Contents/MacOS/$name"

# NSHighResolutionCapable is not optional: without it macOS runs the window through 1x scaling and
# every pixel is blurry on a Retina display, which makes D15's "1:1 is one texel per *physical*
# pixel" look broken when it is not.
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>         <string>$name</string>
    <key>CFBundleIdentifier</key>         <string>com.psmyles.fire.dev</string>
    <key>CFBundleName</key>               <string>$name</string>
    <key>CFBundleDisplayName</key>        <string>$name ($profile)</string>
    <key>CFBundlePackageType</key>        <string>APPL</string>
    <key>CFBundleShortVersionString</key> <string>$version</string>
    <key>CFBundleVersion</key>            <string>$version</string>
    <key>LSMinimumSystemVersion</key>     <string>11.0</string>
    <key>NSHighResolutionCapable</key>    <true/>
</dict>
</plist>
PLIST

echo "bundled $profile build -> $app"

[[ ${#args[@]} -eq 0 ]] && exit 0

# Fire is one process for N windows and coordinates through an instance socket, so launching while
# an older build is still up would forward the open to *that* process and exit — you would be
# testing the binary you just replaced. Retire the previous dev-bundle instance first.
if pkill -f "$app/Contents/MacOS/$name" 2>/dev/null; then
    echo "(stopped the previous $name dev instance so this build is the one that opens)"
    sleep 0.3
fi

# Absolute paths: `open` does not resolve --args against the caller's working directory.
abs=()
for a in "${args[@]}"; do
    [[ "$a" == /* ]] && abs+=("$a") || abs+=("$PWD/$a")
done
open "$app" --args "${abs[@]}"
