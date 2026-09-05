#!/usr/bin/env bash
# Build, sign, notarize and package Fire for macOS — the twin of scripts/build-installer.ps1.
#
# This runs **on the dev Mac, by hand** (D11). CI builds and tests the mac leg but never signs or
# packages, so the Developer ID certificate and the App Store Connect key never have to exist as
# secrets on a public repo.
#
#   scripts/build-mac.sh                          a release: build, sign, notarize, staple, .dmg
#   scripts/build-mac.sh --no-notarize            signed but not notarized (faster; not shippable)
#   scripts/build-mac.sh --no-sign                unsigned bundle, for local testing only
#   scripts/build-mac.sh --no-dmg --no-build      re-wrap the binary that is already built
#
# The no-argument form is the shipping one, and takes both of its credentials from the keychain:
# the "Developer ID Application" identity, and the `notarytool` profile named below. Neither is
# ever passed on a command line, and neither lives in the repo.
#
# Options
#   --sign-id <identity>       codesign identity; default is the "Developer ID Application" one
#                              in the keychain, which is the only kind Gatekeeper accepts for
#                              distribution outside the App Store.
#   --no-sign                  skip codesign entirely. The bundle runs here and nowhere else:
#                              another Mac will refuse it. Implies --no-notarize. Never ship this.
#   --notarize-profile <name>  use a different `xcrun notarytool store-credentials` profile.
#   --no-notarize              skip notarization and stapling. The .dmg is still signed, but
#                              macOS 15+ has no Control-click bypass any more — opening it on
#                              another Mac means System Settings → Privacy & Security → Open
#                              Anyway. Use it to iterate, not to ship.
#   --no-dmg                   stop after the .app.
#   --no-build                 reuse target/release/fire as it stands.
#   --out <dir>                where the .dmg goes (default: dist/).
#
# What is deliberately not here: a universal binary. D10 is Apple Silicon only, and the vendored
# HEIF stack is arm64-only, so the script checks the binary's architecture rather than pretending.
#
# One thing to know while testing: a dev bundle (scripts/dev-app.sh, `com.psmyles.fire.dev`) and
# this one share the *instance socket* name, because that name is the product's, not the bundle's.
# Launching one while the other runs forwards the open to whichever got there first. Quit the dev
# build before testing this one.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sign_id=""
do_sign=1
do_notarize=1
do_dmg=1
do_build=1
out_dir="$repo/dist"

usage() { sed -n '2,38p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign-id)           sign_id="$2"; shift 2 ;;
        --no-sign)           do_sign=0; shift ;;
        --notarize-profile)  notary_profile="$2"; shift 2 ;;
        --no-notarize)       do_notarize=0; shift ;;
        --no-dmg)            do_dmg=0; shift ;;
        --no-build)          do_build=0; shift ;;
        --out)               out_dir="$2"; shift 2 ;;
        -h|--help)           usage; exit 0 ;;
        *)                   echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

say() { printf '\n== %s ==\n' "$1"; }
die() { echo "error: $*" >&2; exit 1; }

# Product metadata comes from product.json, the same single source build.rs reads, so the bundle
# cannot drift from the binary it wraps. plutil reads JSON, so this needs nothing installed.
name="$(plutil -extract productName raw -o - "$repo/product.json")"
version="$(plutil -extract version raw -o - "$repo/product.json")"
copyright="$(plutil -extract copyright raw -o - "$repo/product.json")"
bundle_id="com.psmyles.fire"
# assets/icon.png has a transparent background; the Dock, Finder and the app switcher all show it
# against whatever is behind them, which leaves the flame floating. Composite it onto this instead.
icon_bg="343639"
# ...inset by this much of the icon's edge on every side. The master's artwork runs corner to
# corner, and macOS 26 masks a legacy icon like this one into the rounded-rect it draws everywhere,
# so with no inset the flame is clipped at the top and bottom and crowds the curve at the sides.
icon_inset="6%"
# The `xcrun notarytool store-credentials` profile to use when none is named. It is the product
# name, lower-cased, which is what this script's own setup instructions tell you to create. The
# credential itself lives in the data-protection keychain — `security(1)` cannot even enumerate
# it, which is also why there is no point pre-flighting its existence here: notarytool's own
# error is immediate and says exactly what is missing.
notary_profile="${notary_profile:-$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')}"

# `--no-sign` leaves nothing notarizable: notarization is a check on a Developer ID signature.
[[ $do_sign -eq 1 ]] || do_notarize=0

# Submit `path` and wait. On failure, print the one command that says *why* — notarytool's exit
# status alone does not, and `set -e` would otherwise kill the script before the ID is visible.
notarize() {
    local path="$1" log status id
    log="$(mktemp)"
    xcrun notarytool submit "$path" --keychain-profile "$notary_profile" --wait 2>&1 | tee "$log"
    status=${PIPESTATUS[0]}
    if [[ $status -ne 0 ]]; then
        id="$(sed -n 's/^ *id: *\([0-9a-f-]*\)$/\1/p' "$log" | head -1)"
        echo >&2
        if [[ -n $id ]]; then
            echo "error: notarization failed. Apple's reasons:" >&2
            echo "  xcrun notarytool log $id --keychain-profile $notary_profile" >&2
        else
            echo "error: notarization failed before submitting — is the '$notary_profile' profile set up?" >&2
            echo "  xcrun notarytool store-credentials $notary_profile --apple-id <id> --team-id <team> --password <app-specific-password>" >&2
        fi
        rm -f "$log"
        exit 1
    fi
    rm -f "$log"
}

# ---------------------------------------------------------------------------------------------
# 1. Build
# ---------------------------------------------------------------------------------------------

if [[ $do_build -eq 1 ]]; then
    say "cargo build --release"
    cargo build --manifest-path "$repo/Cargo.toml" -p fire --release
fi

bin="$repo/target/release/fire"
[[ -x "$bin" ]] || die "no release binary at $bin — drop --no-build?"

archs="$(lipo -archs "$bin")"
[[ "$archs" == *arm64* ]] || die "the binary is '$archs', not arm64 (D10 is Apple Silicon only)"

# ---------------------------------------------------------------------------------------------
# 2. The icon
# ---------------------------------------------------------------------------------------------
# iconutil wants an .iconset directory of exact sizes, each one the 1024² master downsampled,
# inset by `icon_inset` and composited onto the opaque `icon_bg`. Rebuilt every run: it is well
# under a second and it means a change to assets/icon.png cannot be silently missing from a
# release.

say "icon"
tmp="$(mktemp -d)"
iconset="$tmp/$name.iconset"
mkdir -p "$iconset"

# `sips -z` would do the downsampling, but it cannot composite, and a transparent icon is exactly
# what we are trying not to ship. So the resize and the fill happen together, in AppKit, driven by
# osascript's JavaScript-for-Automation ObjC bridge — which is in the base OS, so this still needs
# nothing installed.
#
# The scratch bitmap is retagged sRGB before anything is drawn into it. Without that it is
# NSCalibratedRGB, and both the fill and the artwork come out colour-converted: #343639 lands as
# #27292b. Retagged, the background is exactly the hex asked for and the flame's pixels match the
# master's byte for byte.
cat > "$tmp/flatten-icon.js" <<'JS'
ObjC.import('AppKit');

// argv: <src.png> <rrggbb> <inset-percent> <size>:<dst.png>...
function run(argv) {
    var src = argv[0], hex = argv[1], inset = parseFloat(argv[2]) / 100;
    var img = $.NSImage.alloc.initWithContentsOfFile(src);
    if (img.isNil()) throw new Error('cannot read ' + src);
    var rgb = [0, 2, 4].map(function (i) { return parseInt(hex.substr(i, 2), 16) / 255; });

    argv.slice(3).forEach(function (spec) {
        var colon = spec.indexOf(':');
        var size = parseInt(spec.slice(0, colon), 10), dst = spec.slice(colon + 1);
        // Fractional at the small sizes (16² insets by under a pixel), which is the point: the
        // artwork is drawn into a sub-pixel rect and antialiased, rather than snapped to the edge.
        var pad = size * inset;

        var rep = $.NSBitmapImageRep.alloc
            .initWithBitmapDataPlanesPixelsWidePixelsHighBitsPerSampleSamplesPerPixelHasAlphaIsPlanarColorSpaceNameBytesPerRowBitsPerPixel(
                $(), size, size, 8, 4, true, false, $.NSCalibratedRGBColorSpace, 0, 0);
        rep = rep.bitmapImageRepByRetaggingWithColorSpace($.NSColorSpace.sRGBColorSpace);

        $.NSGraphicsContext.saveGraphicsState;
        $.NSGraphicsContext.setCurrentContext(
            $.NSGraphicsContext.graphicsContextWithBitmapImageRep(rep));
        $.NSGraphicsContext.currentContext.setImageInterpolation(3); // .high
        $.NSColor.colorWithSRGBRedGreenBlueAlpha(rgb[0], rgb[1], rgb[2], 1.0).setFill;
        $.NSBezierPath.fillRect($.NSMakeRect(0, 0, size, size));
        img.drawInRectFromRectOperationFraction(
            $.NSMakeRect(pad, pad, size - 2 * pad, size - 2 * pad),
            $.NSZeroRect, $.NSCompositingOperationSourceOver, 1.0);
        $.NSGraphicsContext.restoreGraphicsState;

        var png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
        if (!png.writeToFileAtomically(dst, true)) throw new Error('cannot write ' + dst);
    });
}
JS

renders=()
for size in 16 32 128 256 512; do
    renders+=("$size:$iconset/icon_${size}x${size}.png")
    renders+=("$((size * 2)):$iconset/icon_${size}x${size}@2x.png")
done
osascript -l JavaScript "$tmp/flatten-icon.js" \
    "$repo/assets/icon.png" "$icon_bg" "${icon_inset%\%}" "${renders[@]}" \
    || die "could not render the iconset from assets/icon.png"
(( $(ls "$iconset" | wc -l) == ${#renders[@]} )) || die "the iconset is short of ${#renders[@]} images"

icns="$tmp/$name.icns"
iconutil -c icns "$iconset" -o "$icns"
echo "  $(basename "$icns") — $(stat -f%z "$icns") bytes, on #$icon_bg, inset $icon_inset"

# ---------------------------------------------------------------------------------------------
# 3. The bundle
# ---------------------------------------------------------------------------------------------

app="$repo/target/$name.app"
say "bundle → $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/$name"
cp "$icns" "$app/Contents/Resources/$name.icns"
# The four-byte type/creator file. Vestigial, but its absence still confuses some tools.
printf 'APPL????' > "$app/Contents/PkgInfo"

# The file types Finder will offer Fire for. Read straight out of `SUPPORTED_EXTENSIONS` in
# fire-decode — *the* extension table — so this cannot drift from what the app can actually open.
# `installer/fire.iss` is the one consumer that has to keep its own copy (it is an Inno Setup
# script and can import nothing), which is why a test asserts it matches; parsing the table here
# means this file needs no such test.
extensions=()
while IFS= read -r ext; do extensions+=("$ext"); done < <(
    awk '/^pub const SUPPORTED_EXTENSIONS/{f=1;next} f&&/^\];/{exit} f' \
        "$repo/crates/fire-decode/src/lib.rs" | sed 's|//.*||' | grep -o '"[a-z0-9]*"' | tr -d '"'
)
# A silent parse failure would produce a bundle Finder never offers, which is exactly the kind of
# thing nobody notices until a colleague reports it. Fail loudly instead.
(( ${#extensions[@]} >= 40 )) || die "only ${#extensions[@]} extensions parsed from fire-decode — the table's shape must have changed"
for required in png exr cr2; do
    [[ " ${extensions[*]} " == *" $required "* ]] || die "extension table parse looks wrong: no .$required"
done
echo "  ${#extensions[@]} file extensions"

ext_xml=""
for ext in "${extensions[@]}"; do
    ext_xml+="                <string>$ext</string>"$'\n'
done

# One document-type entry rather than one per format. Grouping them the way fire.iss does would
# buy nothing on macOS: with `LSHandlerRank = Alternate` we do not own the UTI, so the per-type
# name never surfaces in Finder — and it would mean a second, hand-maintained list to drift.
#
# `LSHandlerRank = Alternate` (rather than Owner) is deliberate: Fire volunteers for these files
# and appears in "Open With", but does not take .png away from Preview on install. The user
# chooses, in Get Info → Change All.
#
# Note what is *not* declared: `NSSupportsSuddenTermination`. It would let the OS SIGKILL Fire on
# quit, skipping the `atexit` that removes the instance socket — the exact 2-second stall step 7
# went and fixed.
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>         <string>$name</string>
    <key>CFBundleIdentifier</key>         <string>$bundle_id</string>
    <key>CFBundleName</key>               <string>$name</string>
    <key>CFBundleDisplayName</key>        <string>$name</string>
    <key>CFBundleIconFile</key>           <string>$name</string>
    <key>CFBundlePackageType</key>        <string>APPL</string>
    <key>CFBundleShortVersionString</key> <string>$version</string>
    <key>CFBundleVersion</key>            <string>$version</string>
    <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
    <key>NSHumanReadableCopyright</key>   <string>$copyright</string>
    <key>LSApplicationCategoryType</key>  <string>public.app-category.graphics-design</string>
    <!-- arm64 only (D10), and arm64 starts at Big Sur. -->
    <key>LSMinimumSystemVersion</key>     <string>11.0</string>
    <!-- Not optional: without it macOS runs the window through 1x scaling and every pixel is
         blurry on a Retina display, which makes D15's "1:1 is one texel per physical pixel" look
         broken when it is not. -->
    <key>NSHighResolutionCapable</key>    <true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>       <string>Image</string>
            <key>CFBundleTypeRole</key>       <string>Viewer</string>
            <key>LSHandlerRank</key>          <string>Alternate</string>
            <key>CFBundleTypeIconFile</key>   <string>$name</string>
            <key>CFBundleTypeExtensions</key>
            <array>
$ext_xml            </array>
        </dict>
    </array>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null || die "generated Info.plist is malformed"
echo "  $app"

# ---------------------------------------------------------------------------------------------
# 4. Sign
# ---------------------------------------------------------------------------------------------

if [[ $do_sign -eq 1 ]]; then
    if [[ -z $sign_id ]]; then
        # Only a "Developer ID Application" certificate produces something another Mac will run;
        # an "Apple Development" one signs fine here and is rejected everywhere else, so picking
        # one automatically would just move the failure to the colleague.
        sign_id="$(security find-identity -v -p codesigning \
            | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)"
        [[ -n $sign_id ]] || die "no 'Developer ID Application' identity in the keychain.
  Install one from developer.apple.com, or pass --sign-id, or --no-sign for a local-only build."
    fi
    say "codesign — $sign_id"
    # `--options runtime` is the hardened runtime, which notarization requires; `--timestamp` gets
    # a secure timestamp from Apple, which it also requires and which is what keeps the signature
    # valid after the certificate expires. No entitlements: Fire needs none, and every entitlement
    # is a hole to justify.
    codesign --force --options runtime --timestamp \
        --sign "$sign_id" "$app"
    codesign --verify --deep --strict --verbose=2 "$app"
else
    say "codesign — skipped (--no-sign)"
    echo "  This bundle will run on this Mac and be refused on every other one."
fi

# ---------------------------------------------------------------------------------------------
# 5. Notarize the app
# ---------------------------------------------------------------------------------------------
# Stapling the *app* as well as the .dmg matters: a stapled ticket is what lets it launch on a Mac
# that is offline or behind a filter. Without it Gatekeeper has to reach Apple on first launch,
# and the failure looks like "damaged and can't be opened" rather than "no network".

if [[ $do_notarize -eq 1 ]]; then
    say "notarize $name.app — profile '$notary_profile'"
    zip_path="$(mktemp -d)/$name.zip"
    # ditto, not zip(1): only ditto preserves the bundle's symlinks and extended attributes, and
    # notarytool rejects an archive that has lost them.
    ditto -c -k --keepParent "$app" "$zip_path"
    notarize "$zip_path"
    xcrun stapler staple "$app"
    xcrun stapler validate "$app"
fi

# ---------------------------------------------------------------------------------------------
# 6. The disk image
# ---------------------------------------------------------------------------------------------

if [[ $do_dmg -eq 0 ]]; then
    say "done — $app"
    exit 0
fi

say "dmg"
mkdir -p "$out_dir"
dmg="$out_dir/$name-$version.dmg"
staging="$(mktemp -d)/$name"
mkdir -p "$staging"
# ditto again, for the same reason: `cp -R` on a signed bundle can drop extended attributes and
# invalidate the signature.
ditto "$app" "$staging/$name.app"
# The drag-to-install target. A .dmg with only the app in it teaches people to run it from the
# disk image, where it stays read-only and every update re-downloads.
ln -s /Applications "$staging/Applications"
rm -f "$dmg"
# `hdiutil` prints a deprecation notice on macOS 26+, pointing at `diskutil image create`. It is
# left as it is on purpose: the replacement does not exist on any earlier macOS, and a release
# script that only runs on the newest one is worse than a warning. UDZO is the compressed,
# read-only format every Mac can mount.
hdiutil create -volname "$name $version" -srcfolder "$staging" -ov -format UDZO "$dmg" >/dev/null
echo "  $dmg — $(stat -f%z "$dmg") bytes"

if [[ $do_sign -eq 1 ]]; then
    codesign --force --timestamp --sign "$sign_id" "$dmg"
fi

if [[ $do_notarize -eq 1 ]]; then
    say "notarize $(basename "$dmg")"
    notarize "$dmg"
    xcrun stapler staple "$dmg"
fi

# ---------------------------------------------------------------------------------------------
# 7. The .dmg's own Finder icon
# ---------------------------------------------------------------------------------------------
# Without this the .dmg gets the generic disk-image document icon. What goes on instead is that
# same generic icon with the flame composited over its disk graphic, so it still reads as a disk
# image at a glance and is unmistakably ours. The base comes from NSWorkspace rather than a
# checked-in PNG, so it is whatever the running macOS draws for a .dmg and cannot go stale.
#
# This runs *last*, after stapling, because a custom file icon is not in the file's data: it is a
# resource fork in the `com.apple.ResourceFork` xattr plus a flag in `com.apple.FinderInfo`, and
# codesign, notarytool and stapler all rewrite the .dmg. Applied here it costs the signature
# nothing — section 8 re-checks all three to prove it.
#
# Know one limit before relying on it: xattrs travel only where the transport carries them, which
# means AirDrop, a `ditto`-made zip, or a copy between Macs. A plain HTTPS download strips the
# fork and the .dmg lands generic again. This is cosmetic-on-your-Mac, not branding for the web.

say "dmg icon"
dmg_iconset="$tmp/dmg.iconset"
mkdir -p "$dmg_iconset"
cat > "$tmp/dmg-icon.js" <<'JS'
ObjC.import('AppKit');

// The flame's edge, and its centre measured from the top-left, as fractions of the icon's edge.
// It replaces the generic download arrow, and it has to stay inside the disk graphic drawn on the
// document — the flame overhanging that frame reads as a mistake rather than as a badge.
//
// The numbers are fitted to the disk's interior, which at 512² is x 162..350 and y 150..344 (344
// being the top of the darker bar along its bottom). The master's artwork is full-bleed
// vertically — its ink is 1020 of 1024 tall, and 690 wide — so height is what binds: 0.34 puts
// the ink 174 tall inside a 194-tall opening, leaving ~11px top and bottom at 512² and much more
// at the sides. Raising it much past 0.35 starts clipping the flame's tip over the disk's edge.
var LOGO_SCALE = 0.34, LOGO_CX = 0.5, LOGO_CY = 0.484;

// argv: <logo.png> <size>:<dst.png>...
function run(argv) {
    var img = $.NSImage.alloc.initWithContentsOfFile(argv[0]);
    if (img.isNil()) throw new Error('cannot read ' + argv[0]);
    // Deprecated in favour of -iconForContentType:, which does not bridge cleanly through JXA.
    // It still returns the current system artwork, with reps up to 2048², so 1024² is a downsample
    // rather than an upscale.
    var base = $.NSWorkspace.sharedWorkspace.iconForFileType('dmg');

    argv.slice(1).forEach(function (spec) {
        var colon = spec.indexOf(':');
        var size = parseInt(spec.slice(0, colon), 10), dst = spec.slice(colon + 1);

        var rep = $.NSBitmapImageRep.alloc
            .initWithBitmapDataPlanesPixelsWidePixelsHighBitsPerSampleSamplesPerPixelHasAlphaIsPlanarColorSpaceNameBytesPerRowBitsPerPixel(
                $(), size, size, 8, 4, true, false, $.NSCalibratedRGBColorSpace, 0, 0);
        rep = rep.bitmapImageRepByRetaggingWithColorSpace($.NSColorSpace.sRGBColorSpace);

        $.NSGraphicsContext.saveGraphicsState;
        $.NSGraphicsContext.setCurrentContext(
            $.NSGraphicsContext.graphicsContextWithBitmapImageRep(rep));
        $.NSGraphicsContext.currentContext.setImageInterpolation(3); // .high
        base.drawInRectFromRectOperationFraction(
            $.NSMakeRect(0, 0, size, size), $.NSZeroRect, $.NSCompositingOperationSourceOver, 1.0);
        var e = size * LOGO_SCALE;
        img.drawInRectFromRectOperationFraction(
            $.NSMakeRect(size * LOGO_CX - e / 2, size * (1 - LOGO_CY) - e / 2, e, e),
            $.NSZeroRect, $.NSCompositingOperationSourceOver, 1.0);
        $.NSGraphicsContext.restoreGraphicsState;

        var png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
        if (!png.writeToFileAtomically(dst, true)) throw new Error('cannot write ' + dst);
    });
}
JS

dmg_renders=()
for size in 16 32 128 256 512; do
    dmg_renders+=("$size:$dmg_iconset/icon_${size}x${size}.png")
    dmg_renders+=("$((size * 2)):$dmg_iconset/icon_${size}x${size}@2x.png")
done
osascript -l JavaScript "$tmp/dmg-icon.js" "$repo/assets/icon.png" "${dmg_renders[@]}" \
    || die "could not render the .dmg icon"
(( $(ls "$dmg_iconset" | wc -l) == ${#dmg_renders[@]} )) || die "the .dmg iconset is short of images"
dmg_icns="$tmp/dmg.icns"
iconutil -c icns "$dmg_iconset" -o "$dmg_icns"

# -setIcon:forFile:options: writes the fork and sets the flag in one call, which is the whole
# reason not to do this with Rez and SetFile.
osascript -l JavaScript -e '
    ObjC.import("AppKit");
    function run(argv) {
        var img = $.NSImage.alloc.initWithContentsOfFile(argv[0]);
        if (img.isNil()) throw new Error("cannot read " + argv[0]);
        if (!$.NSWorkspace.sharedWorkspace.setIconForFileOptions(img, argv[1], 0))
            throw new Error("setIcon:forFile: refused " + argv[1]);
    }' "$dmg_icns" "$dmg" || die "could not set the .dmg's Finder icon"
echo "  applied to $(basename "$dmg")"

# ---------------------------------------------------------------------------------------------
# 8. Verify what actually ships
# ---------------------------------------------------------------------------------------------
# Deliberately after the icon, not before: these have to be true of the bytes that leave the Mac,
# and the icon is the last thing to touch them.

if [[ $do_sign -eq 1 ]]; then
    say "verify"
    codesign --verify --verbose=2 "$dmg"
fi

if [[ $do_notarize -eq 1 ]]; then
    xcrun stapler validate "$dmg"
    # The question the user's Mac will actually ask. `spctl` here is the same check Gatekeeper
    # runs on first launch, so a pass means the download will open with a double-click.
    say "gatekeeper"
    spctl -a -t open --context context:primary-signature -vv "$dmg"
fi

say "done"
echo "  app: $app"
echo "  dmg: $dmg"
if [[ $do_notarize -eq 0 ]]; then
    echo
    echo "  NOT NOTARIZED — do not ship this. On another Mac it opens only via System Settings →"
    if [[ $do_sign -eq 0 ]]; then
        echo "  Privacy & Security → Open Anyway, and unsigned it may not open at all."
    else
        echo "  Privacy & Security → Open Anyway (macOS 15 removed the Control-click bypass)."
    fi
    echo "  Re-run without --no-notarize / --no-sign for a shippable build."
fi
