#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "$0")/.." && pwd)"
app_dir="$project_root/dist/Schedule Manager.app"
contents_dir="$app_dir/Contents"
app_version="${APP_VERSION:-1.0.3}"

cd "$project_root"
cargo build --release --locked --bin schedule-manager

rm -rf "$app_dir"
mkdir -p "$contents_dir/MacOS" "$contents_dir/Resources"
cp "$project_root/target/release/schedule-manager" "$contents_dir/MacOS/ScheduleManager"

iconset="$project_root/target/ScheduleManager.iconset"
rm -rf "$iconset"
mkdir -p "$iconset"
sips -z 16 16     "$project_root/assets/schedule-logo.png" --out "$iconset/icon_16x16.png" >/dev/null
sips -z 32 32     "$project_root/assets/schedule-logo.png" --out "$iconset/icon_16x16@2x.png" >/dev/null
sips -z 32 32     "$project_root/assets/schedule-logo.png" --out "$iconset/icon_32x32.png" >/dev/null
sips -z 64 64     "$project_root/assets/schedule-logo.png" --out "$iconset/icon_32x32@2x.png" >/dev/null
sips -z 128 128   "$project_root/assets/schedule-logo.png" --out "$iconset/icon_128x128.png" >/dev/null
sips -z 256 256   "$project_root/assets/schedule-logo.png" --out "$iconset/icon_128x128@2x.png" >/dev/null
sips -z 256 256   "$project_root/assets/schedule-logo.png" --out "$iconset/icon_256x256.png" >/dev/null
sips -z 512 512   "$project_root/assets/schedule-logo.png" --out "$iconset/icon_256x256@2x.png" >/dev/null
sips -z 512 512   "$project_root/assets/schedule-logo.png" --out "$iconset/icon_512x512.png" >/dev/null
sips -z 1024 1024 "$project_root/assets/schedule-logo.png" --out "$iconset/icon_512x512@2x.png" >/dev/null
iconutil -c icns "$iconset" -o "$contents_dir/Resources/schedule-logo.icns"
rm -rf "$iconset"

cat > "$contents_dir/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDisplayName</key><string>Schedule Manager</string>
    <key>CFBundleExecutable</key><string>ScheduleManager</string>
    <key>CFBundleIdentifier</key><string>com.emssion.schedule-manager</string>
    <key>CFBundleIconFile</key><string>schedule-logo.icns</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>Schedule Manager</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$app_version</string>
    <key>CFBundleVersion</key><string>${GITHUB_RUN_NUMBER:-1}</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSUserNotificationAlertStyle</key><string>alert</string>
</dict>
</plist>
PLIST

echo "macOS app bundle ready: $app_dir"
