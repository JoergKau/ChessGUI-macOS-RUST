#!/bin/zsh
# package-macos.sh — build SiliconBoard and assemble the macOS .app bundle.
#
# The steps are the manual ones from the README, scripted:
#
#   1. `cargo build --release`
#   2. assemble SiliconBoard.app in the repository root: the binary in
#      Contents/MacOS (renamed to `SiliconBoard`), the icon in
#      Contents/Resources, and the asset tree in Contents/Resources/public
#      (where the asset resolver looks first in a bundle — unit-tested
#      in src/assets.rs),
#   3. write Contents/Info.plist with the version from Cargo.toml and the
#      About-panel copyright (macos_about.rs reads both from the bundle),
#   4. ad-hoc sign the bundle,
#   5. build the distributable zip: SiliconBoard-<version>-macos-arm64.zip
#      holds one folder with the signed app plus LICENSE and README.md,
#      and the SHA-256 is printed for the release listing.
#
# By default the freshly signed bundle is also installed into /Applications
# (replacing an existing SiliconBoard.app), so the daily-use app is always
# the one just built; the dev-folder bundle is then removed and its Launch
# Services entry unregistered, so Launchpad only ever shows one app icon —
# the one in /Applications. Installing is the default because a build left
# in the dev folder gets registered by Launch Services as well and users end
# up with two icons. `--no-install` (or `-n`) skips the install
# step and leaves the bundle in the repo folder.
#
# The .app is a build artefact; it is listed in .gitignore. The distribution
# zip is committed to the repository.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$REPO_ROOT"

APP="SiliconBoard.app"
CONTENTS="$REPO_ROOT/$APP/Contents"
PLIST="$CONTENTS/Info.plist"

version="$(sed -n 's/^version = "\([^\"]*\)"$/\1/p' Cargo.toml | head -1)"
[[ -n "$version" ]] || { print -u2 "No version found in Cargo.toml."; exit 1; }

print "▶ cargo build --release"
cargo build --release

print "▶ Assembling $APP (v$version)"
rm -rf "$REPO_ROOT/$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp target/release/siliconboard "$CONTENTS/MacOS/SiliconBoard"
# Cargo keeps the symbol table in the release profile so dev/internal builds
# give useful crash reports; only the bundle that ships is stripped.
strip -x "$CONTENTS/MacOS/SiliconBoard"
cp assets/icon/SiliconBoard.icns "$CONTENTS/Resources/"
cp -R public "$CONTENTS/Resources/public"
# The start-up PGN rides along, so the installed app also opens
# PGN/WorldChamp1972.pgn at launch and selects game 1.
STARTUP_PGN="$REPO_ROOT/PGN/WorldChamp1972.pgn"
if [[ -f "$STARTUP_PGN" ]]; then
    mkdir -p "$CONTENTS/Resources/PGN"
    cp "$STARTUP_PGN" "$CONTENTS/Resources/PGN/"
else
    print -u2 "  ▲ $STARTUP_PGN is missing — the bundle starts without the start-up PGN."
fi

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>               <string>SiliconBoard</string>
    <key>CFBundleDisplayName</key>        <string>SiliconBoard</string>
    <key>CFBundleExecutable</key>         <string>SiliconBoard</string>
    <key>CFBundleIdentifier</key>         <string>org.chess-gui.SiliconBoard</string>
    <key>CFBundlePackageType</key>        <string>APPL</string>
    <key>CFBundleShortVersionString</key> <string>$version</string>
    <key>CFBundleVersion</key>            <string>$version</string>
    <key>CFBundleIconFile</key>           <string>SiliconBoard.icns</string>
    <key>LSMinimumSystemVersion</key>     <string>11.0</string>
    <key>NSHighResolutionCapable</key>    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>Copyright © 2026 Joerg Kauschke. MIT License.</string>
</dict>
</plist>
EOF
plutil -lint "$PLIST"

print "▶ Ad-hoc signature"
codesign --force --deep --sign - "$APP"

# The distributable zip: the app plus a copy of README and LICENSE (so a
# user who only downloads the zip still gets the MIT text). The zip holds
# one folder, `SiliconBoard-<version>`, and the app is signed before it is
# zipped — a zip added to a signed app would break the seal.
print "▶ Distribution zip"
stage="$REPO_ROOT/target/dist/SiliconBoard-$version"
rm -rf "$(dirname "$stage")"
mkdir -p "$stage"
cp -R "$REPO_ROOT/$APP" "$stage/"
cp LICENSE README.md "$stage/"
zip_file="$REPO_ROOT/SiliconBoard-$version-macos-arm64.zip"
rm -f "$zip_file"
ditto -c -k --sequesterRsrc --keepParent "$stage" "$zip_file"
rm -rf "$(dirname "$stage")"
print "  SHA-256 of $zip_file:"
shasum -a 256 "$zip_file"

LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

print "▶ Installing into /Applications (default — the daily-use app is the one just built)"

# After the bundle is in /Applications: unregister the dev-folder copy (so
# Launch Services cannot show two app entries — one pointing into the dev
# folder), remove that copy, and make the installed copy the authoritative
# registration, so the app that opens is the one just built.
apply_install() {
    "$LSREGISTER" -u "$REPO_ROOT/$APP" 2>/dev/null || true
    rm -rf "$REPO_ROOT/$APP"
    "$LSREGISTER" -f /Applications/"$APP" 2>/dev/null || true
    print "  ✓ /Applications/$APP — the app lives only in /Applications."
}

# `--no-install` skips the step (zip-only run); the old `--install`/`-i`
# flags are still accepted and mean the same as the default.
skip_install=false
case "${1:-}" in
    "" | --install | -i)  ;;
    --no-install | -n)    skip_install=true ;;
    *) print -u2 "Unknown argument: ${1} (usage: $0 [--no-install])"; exit 1 ;;
esac

if [[ "$skip_install" == true ]]; then
    print "  (--no-install) the bundle stays at $REPO_ROOT/$APP and Launchpad shows it as an app."
    print "▶ Done: $zip_file"
    exit 0
fi

rm -rf /Applications/"$APP" 2>/dev/null || true
if ditto "$REPO_ROOT/$APP" /Applications/"$APP"; then
    apply_install
else
    # /Applications is normally writable by an admin user; when it is not,
    # ask for administrator rights once (a macOS password prompt).
    print "  No write access to /Applications; asking for administrator rights…"
    if osascript -e "do shell script \"rm -rf '/Applications/$APP'; /usr/bin/ditto '$REPO_ROOT/$APP' '/Applications/$APP'\" with administrator privileges" >/dev/null 2>&1; then
        apply_install
    else
        print -u2 "  ✗ Could not install (cancelled or no permission)."
        print -u2 "    The bundle stays at $REPO_ROOT/$APP; drag it to /Applications by hand."
    fi
fi
print "▶ Done: $zip_file"
