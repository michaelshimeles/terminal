#!/usr/bin/env bash
# Wrap a release binary in a minimal macOS .app bundle (retina-capable, dark-mode aware), written
# next to the binary as Terminal.app: target/release/terminal unless another path is given.
# The executable inside is named Terminal too, since the app menu's About, Hide and Quit items
# take the process name. The bundle embeds Sparkle, which updates installed copies; it is left
# unsigned, and scripts/release-macos.sh signs it.
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
bin=${1:-target/release/terminal}
app=$(dirname "$bin")/Terminal.app
# The bundle ID keeps the project's old name. Released copies have it, and macOS ties the app's
# Keychain access, privacy permissions and preferences (Sparkle's among them) to it, so changing
# it would make the next update a different app as far as macOS is concerned. Nothing shows it.
bundle_id=dev.terminal-rs.app
# Sparkle checks this feed and installs only updates signed with the private half of this key,
# which `generate_keys --account terminal-rs` keeps in the release Mac's login keychain. Every
# release must be signed with the same key.
feed=https://github.com/michaelshimeles/terminal/releases/latest/download/appcast.xml
sparkle_key=Kq1jkRyRLHyfeNeAv3Fl7we7ARPnyuPh+MI/XSZV77U=
sparkle=$(scripts/fetch-sparkle.sh)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Frameworks"
cp "$bin" "$app/Contents/MacOS/Terminal"
cp app/assets/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"
# The app isn't sandboxed, so Sparkle's XPC services aren't needed; headers and modules are
# build-time only.
framework=$app/Contents/Frameworks/Sparkle.framework
ditto "$sparkle/Sparkle.framework" "$framework"
for part in XPCServices Headers PrivateHeaders Modules; do
  rm -rf "${framework:?}/$part" "${framework:?}/Versions/B/$part"
done
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Terminal</string>
  <key>CFBundleDisplayName</key><string>Terminal</string>
  <key>CFBundleIdentifier</key><string>${bundle_id}</string>
  <key>CFBundleExecutable</key><string>Terminal</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.finance</string>
  <key>SUFeedURL</key><string>${feed}</string>
  <key>SUPublicEDKey</key><string>${sparkle_key}</string>
  <key>SURequireSignedFeed</key><true/>
  <key>SUVerifyUpdateBeforeExtraction</key><true/>
  <key>SUEnableAutomaticChecks</key><true/>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null
echo "bundled $app"
