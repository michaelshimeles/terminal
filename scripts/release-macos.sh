#!/usr/bin/env bash
# Build a universal (Apple Silicon + Intel) Terminal.app, sign it with a Developer ID
# certificate and the hardened runtime, notarize and staple it, then package it as
# target/universal-apple-darwin/release/Terminal.dmg, itself signed, notarized and stapled, with
# the app icon as its volume icon. That file name is the release asset the download page links
# to. Next to it go Terminal-<version>.zip, the app alone, and appcast.xml, the signed update feed
# that points installed copies at that zip. Upload all three to a release tagged v<version>
# (the script prints the command); bump the version in Cargo.toml first, since installed
# copies only offer a version higher than their own.
#
# Notarization credentials come from a notarytool keychain profile. Create it once with an
# app-specific password from account.apple.com and the team ID shown in the certificate name:
#
#   xcrun notarytool store-credentials terminal-rs-notary --apple-id you@example.com --team-id TEAMID
#
# Updates are signed with the EdDSA key in the login keychain under the account terminal-rs,
# whose public half is in scripts/bundle-macos.sh. Keep an exported copy somewhere safe:
# installed copies accept no other key, so losing it ends their updates.
#
#   target/sparkle/<version>/bin/generate_keys --account terminal-rs -x terminal-rs-sparkle.key
#   target/sparkle/<version>/bin/generate_keys --account terminal-rs -f terminal-rs-sparkle.key  # on a new Mac
#
#   SIGN_IDENTITY    signing identity (default: the first "Developer ID Application" in the keychain)
#   NOTARY_PROFILE   notarytool keychain profile (default: terminal-rs-notary)
#   NOTARY_KEYCHAIN  keychain holding that profile (default: the login keychain)
#   SPARKLE_ACCOUNT  login keychain account of the update signing key (default: terminal-rs)
#   NOTARIZE=0       sign and package without notarizing or writing the update feed
set -euo pipefail
cd "$(dirname "$0")/.."

identity=${SIGN_IDENTITY:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: .*\)"$/\1/p' | head -1)}
[ -n "$identity" ] || { echo "no Developer ID Application certificate in the keychain; set SIGN_IDENTITY" >&2; exit 1; }
notarize=${NOTARIZE:-1}
notary=(--keychain-profile "${NOTARY_PROFILE:-terminal-rs-notary}")
[ -z "${NOTARY_KEYCHAIN:-}" ] || notary+=(--keychain "$NOTARY_KEYCHAIN")
if [ "$notarize" = 1 ] && ! xcrun notarytool history "${notary[@]}" >/dev/null; then
  echo "no notarytool credentials; create the profile described at the top of $0, or set NOTARIZE=0" >&2
  exit 1
fi

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
repo=https://github.com/michaelshimeles/terminal-rs
sparkle=$(scripts/fetch-sparkle.sh)
sparkle_account=${SPARKLE_ACCOUNT:-terminal-rs}
sparkle_key=$(sed -n 's/^sparkle_key=//p' scripts/bundle-macos.sh)
if [ "$notarize" = 1 ]; then
  key=$("$sparkle/bin/generate_keys" --account "$sparkle_account" -p 2>/dev/null || true)
  if [ "$key" != "$sparkle_key" ]; then
    echo "the login keychain's update signing key (account $sparkle_account) is '$key', but the app trusts $sparkle_key." >&2
    echo "Import the exported key with $sparkle/bin/generate_keys --account $sparkle_account -f <file>; never generate a new one." >&2
    exit 1
  fi
fi

tmp=$(mktemp -d)
# A failure while the DMG's writable image is mounted would otherwise leave it attached, and
# rm -rf would delete into it.
cleanup() {
  if [ -d "$tmp/mnt" ] && [ "$(stat -f %d "$tmp/mnt")" != "$(stat -f %d "$tmp")" ]; then
    hdiutil detach -force "$tmp/mnt" >/dev/null || { echo "left $tmp/rw.dmg mounted at $tmp/mnt" >&2; return; }
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

# Submits $1, waits for Apple's verdict, then staples the ticket to $2.
notarize() {
  local out id
  out=$(xcrun notarytool submit "$1" "${notary[@]}" --wait --output-format json) || true
  if [ "$(plutil -extract status raw -o - - <<<"$out" 2>/dev/null)" != Accepted ]; then
    echo "notarizing $1 failed: $out" >&2
    if id=$(plutil -extract id raw -o - - <<<"$out" 2>/dev/null); then
      xcrun notarytool log "$id" "${notary[@]}" >&2
    fi
    exit 1
  fi
  xcrun stapler staple "$2"
}

for target in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --locked --target "$target" -p terminal-rs
done
out=target/universal-apple-darwin/release
mkdir -p "$out"
rm -f "$out"/Terminal-*.zip "$out/appcast.xml"
lipo -create -output "$out/terminal-rs" \
  target/aarch64-apple-darwin/release/terminal-rs target/x86_64-apple-darwin/release/terminal-rs
scripts/bundle-macos.sh "$out/terminal-rs"
app=$out/Terminal.app
framework=$app/Contents/Frameworks/Sparkle.framework

echo "signing as $identity"
# Inside out, Sparkle's helpers first. The hardened runtime loads Sparkle.framework only when it
# is signed by the app's own team.
for code in "$framework/Versions/B/Autoupdate" "$framework/Versions/B/Updater.app" "$framework" "$app"; do
  codesign --force --options runtime --timestamp --sign "$identity" "$code"
done
codesign --verify --deep --strict --verbose=2 "$app"
if [ "$notarize" = 1 ]; then
  ditto -c -k --keepParent "$app" "$tmp/Terminal.zip"
  notarize "$tmp/Terminal.zip" "$app"
fi

dmg=$out/Terminal.dmg
stage=$tmp/dmg
mkdir "$stage" "$tmp/mnt"
ditto "$app" "$stage/Terminal.app"
ln -s /Applications "$stage/Applications"
cp app/assets/AppIcon.icns "$stage/.VolumeIcon.icns"
# -srcfolder drops the volume's custom-icon flag, so it is set on a writable image that is then
# compressed.
hdiutil create -volname Terminal -srcfolder "$stage" -format UDRW "$tmp/rw.dmg"
hdiutil attach -nobrowse -noautoopen -mountpoint "$tmp/mnt" "$tmp/rw.dmg" >/dev/null
SetFile -a C "$tmp/mnt"
hdiutil detach "$tmp/mnt" >/dev/null
hdiutil convert "$tmp/rw.dmg" -format UDZO -ov -o "$dmg"
codesign --force --timestamp --sign "$identity" "$dmg"
if [ "$notarize" = 1 ]; then
  notarize "$dmg" "$dmg"
  spctl --assess --type execute --verbose=2 "$app"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
fi
echo "built $dmg"

# ditto keeps the signature and the stapled ticket; plain zip does not.
zip=$out/Terminal-$version.zip
ditto -c -k --keepParent "$app" "$zip"
echo "built $zip"
[ "$notarize" = 1 ] || exit 0

# Installed copies read releases/latest/download/appcast.xml (SUFeedURL). generate_keys created
# the key's keychain item, so it can export it without a prompt; the export lives in $tmp only
# while signing.
key_file=$tmp/sparkle.key
"$sparkle/bin/generate_keys" --account "$sparkle_account" -x "$key_file" >/dev/null
# sparkle:edSignature="…" length="…"
enclosure=$("$sparkle/bin/sign_update" --ed-key-file "$key_file" "$zip")
appcast=$out/appcast.xml
cat > "$appcast" <<XML
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>Terminal</title>
    <link>$repo</link>
    <item>
      <title>Terminal $version</title>
      <pubDate>$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S +0000')</pubDate>
      <sparkle:version>$version</sparkle:version>
      <sparkle:shortVersionString>$version</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>11.0</sparkle:minimumSystemVersion>
      <sparkle:fullReleaseNotesLink>$repo/releases/tag/v$version</sparkle:fullReleaseNotesLink>
      <enclosure url="$repo/releases/download/v$version/$(basename "$zip")" type="application/octet-stream" $enclosure/>
    </item>
  </channel>
</rss>
XML
# The app sets SURequireSignedFeed, so the feed carries its own signature as well.
"$sparkle/bin/sign_update" --ed-key-file "$key_file" "$appcast"
"$sparkle/bin/sign_update" --ed-key-file "$key_file" --verify "$appcast"
rm -f "$key_file"
echo "built $appcast"
echo "publish with: gh release create v$version $dmg $zip $appcast"
