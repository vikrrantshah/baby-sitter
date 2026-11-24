#!/bin/sh
# Builds target/release/Baby Sitter.app with both binaries, the bundle Info.plist,
# and an icon rendered from app/assets/icon.svg. Prints the bundle path.
set -eu
cd "$(dirname "$0")/.."

cargo build --release --workspace

app="target/release/Baby Sitter.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# The app starts the CLI from its own directory, so both live in Contents/MacOS.
cp target/release/baby-sitter target/release/baby-sitter-app "$app/Contents/MacOS/"
cp app/macos/Info.plist "$app/Contents/Info.plist"

iconset="$(mktemp -d)/AppIcon.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  double=$((size * 2))
  sips -s format png -z "$size" "$size" app/assets/icon.svg \
    --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -s format png -z "$double" "$double" app/assets/icon.svg \
    --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$iconset")"

# Sign the inner binaries first, then the bundle that seals them.
codesign --force --sign - "$app/Contents/MacOS/baby-sitter"
codesign --force --sign - "$app"

echo "$app"
