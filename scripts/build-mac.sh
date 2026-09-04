#!/usr/bin/env bash
# Build, sign, notarize and package Fire for macOS — the twin of scripts/build-installer.ps1.
#
# This runs **on the dev Mac, by hand** (D11). CI builds and tests the mac leg but never signs or
# packages, so the Developer ID certificate and the App Store Connect key never have to exist as
# secrets on a public repo.
#
#   scripts/build-mac.sh                          build + sign + .dmg (no notarization)
#   scripts/build-mac.sh --notarize-profile fire  ... and notarize + staple both
#   scripts/build-mac.sh --no-sign                unsigned bundle, for local testing only
#   scripts/build-mac.sh --no-dmg --no-build      re-wrap the binary that is already built
#
# Options
#   --sign-id <identity>       codesign identity; default is the "Developer ID Application" one
#                              in the keychain, which is the only kind Gatekeeper accepts for
#                              distribution outside the App Store.
#   --no-sign                  skip codesign entirely. The bundle runs here and nowhere else:
#                              another Mac will refuse it. Never ship this.
#   --notarize-profile <name>  a `xcrun notarytool store-credentials` profile. Without one,
#                              notarization is skipped and the .dmg is signed but not stapled —
#                              fine for a colleague who will right-click → Open, not for a link.
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
notary_profile=""
do_dmg=1
do_build=1
out_dir="$repo/dist"

usage() { sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign-id)           sign_id="$2"; shift 2 ;;
        --no-sign)           do_sign=0; shift ;;
        --notarize-profile)  notary_profile="$2"; shift 2 ;;
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
# iconutil wants an .iconset directory of exact sizes; `sips` downsamples the 1024² master. Both
# ship with macOS, so this needs nothing installed. Rebuilt every run: it is ~50 ms and it means
# a change to assets/icon.png cannot be silently missing from a release.

say "icon"
iconset="$(mktemp -d)/$name.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size "$repo/assets/icon.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) "$repo/assets/icon.png" \
        --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
icns="$(dirname "$iconset")/$name.icns"
iconutil -c icns "$iconset" -o "$icns"
echo "  $(basename "$icns") — $(stat -f%z "$icns") bytes"

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

if [[ -n $notary_profile ]]; then
    [[ $do_sign -eq 1 ]] || die "--notarize-profile needs a signed bundle; drop --no-sign"
    say "notarize $name.app"
    zip_path="$(mktemp -d)/$name.zip"
    # ditto, not zip(1): only ditto preserves the bundle's symlinks and extended attributes, and
    # notarytool rejects an archive that has lost them.
    ditto -c -k --keepParent "$app" "$zip_path"
    xcrun notarytool submit "$zip_path" --keychain-profile "$notary_profile" --wait
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

if [[ -n $notary_profile ]]; then
    say "notarize $(basename "$dmg")"
    xcrun notarytool submit "$dmg" --keychain-profile "$notary_profile" --wait
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
    # The question the user's Mac will actually ask. `spctl` here is the same check Gatekeeper
    # runs on first launch, so a pass means the download will open with a double-click.
    say "gatekeeper"
    spctl -a -t open --context context:primary-signature -vv "$dmg"
fi

say "done"
echo "  app: $app"
echo "  dmg: $dmg"
if [[ -z $notary_profile ]]; then
    echo
    echo "  Not notarized. macOS will refuse this on another Mac until the user right-clicks →"
    echo "  Open. Set up a profile once with:"
    echo "    xcrun notarytool store-credentials fire --apple-id <id> --team-id <team> --password <app-specific-password>"
    echo "  then re-run with --notarize-profile fire."
fi
