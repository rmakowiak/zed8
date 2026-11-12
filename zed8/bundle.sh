#!/bin/sh
# Builds Zed8.app and installs it in /Applications.
#
# Zed8 is a personal build of Zed. It uses the stable channel, so it opens
# your real Zed settings, projects and threads. Auto-update is off, because
# an update would replace Zed8 with official Zed. The zed8-update skill
# updates Zed8 from upstream instead.
#
# Usage: zed8/bundle.sh [--no-install]
set -eu
cd "$(dirname "$0")/.."

install=1
[ "${1:-}" = "--no-install" ] && install=0

# A Claude Code session must not leak into the app's environment.
for var in $(env | grep -Eo "^(CLAUDE[A-Z_]*|AI_AGENT)=" | tr -d =); do unset "$var"; done

export PATH="$HOME/.cargo/bin:$PATH"
export ZED_RELEASE_CHANNEL=stable
export ZED_UPDATE_EXPLANATION="Zed8 updates with the zed8-update skill, not from zed.dev."
# Its own target folder: the two settings above change how most crates compile.
export CARGO_TARGET_DIR="$PWD/target/zed8"

# Without full Xcode, Metal shaders compile at run time.
cargo build --profile release-fast -p zed -p cli --features gpui_platform/runtime_shaders

bin="$CARGO_TARGET_DIR/release-fast"
app="$CARGO_TARGET_DIR/Zed8.app"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/zed/Cargo.toml | head -n 1)
commit=$(git rev-parse --short HEAD)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin/zed" "$app/Contents/MacOS/zed"
cp "$bin/cli" "$app/Contents/MacOS/cli"
# Official Zed ships its own git. Zed falls back to the system git without it.
[ -x /Applications/Zed.app/Contents/MacOS/git ] && cp /Applications/Zed.app/Contents/MacOS/git "$app/Contents/MacOS/git"
cp crates/zed/resources/Document.icns "$app/Contents/Resources/Document.icns"

# The icon: the dev icon, so Zed8 and official Zed look different in the Dock.
iconset="$CARGO_TARGET_DIR/Zed8.iconset"
rm -rf "$iconset" && mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size crates/zed/resources/app-icon-dev@2x.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double crates/zed/resources/app-icon-dev@2x.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Zed8.icns"

# Info.plist: the document types and permissions of official Zed, with the
# Zed8 name, id and version.
plist="$app/Contents/Info.plist"
cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>English</string>
    <key>CFBundleDisplayName</key><string>Zed8</string>
    <key>CFBundleExecutable</key><string>zed</string>
    <key>CFBundleIconFile</key><string>Zed8.icns</string>
    <key>CFBundleIdentifier</key><string>dev.zed.Zed8</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>Zed8</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version+$commit</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>CFBundleURLTypes</key>
    <array><dict><key>CFBundleURLName</key><string>Zed8</string><key>CFBundleURLSchemes</key><array><string>zed</string></array></dict></array>
</dict>
</plist>
EOF
for ext in crates/zed/resources/info/*.plist; do
    # Each file holds plist keys without the outer dict. Merge them in.
    tmp="$CARGO_TARGET_DIR/ext.plist"
    { echo '<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>'; cat "$ext"; echo '</dict></plist>'; } > "$tmp"
    /usr/libexec/PlistBuddy -c "Merge $tmp" "$plist" >/dev/null
done
plutil -lint "$plist" >/dev/null

# Ad hoc signature, with the entitlements of official Zed minus the ones
# that need Zed's Apple team.
entitlements="$CARGO_TARGET_DIR/zed8.entitlements"
sed '/com.apple.developer.associated-domains/,+1d' crates/zed/resources/zed.entitlements > "$entitlements"
codesign --force --deep --entitlements "$entitlements" --sign - "$app" >/dev/null 2>&1

echo "Built $app ($version, $commit)"

if [ "$install" = 1 ]; then
    # Installs now if Zed8 is closed, or as soon as you quit it.
    exec zed8/install.sh
fi
