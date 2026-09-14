#!/bin/sh
# Builds chud and installs it as ~/Applications/chud.app: its own window, fonts bundled.
# Re-run after any code change.
set -e
cd "$(dirname "$0")"
cargo build --release --workspace

APP="$HOME/Applications/chud.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/chud target/release/chud-app "$APP/Contents/MacOS/"
cp app/chud.icns "$APP/Contents/Resources/"   # regenerate with: python3 app/icon.py
cat > "$APP/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>chud</string>
  <key>CFBundleDisplayName</key><string>chud</string>
  <key>CFBundleIdentifier</key><string>dev.chud.app</string>
  <key>CFBundleExecutable</key><string>chud-app</string>
  <key>CFBundleIconFile</key><string>chud</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
codesign --force --deep -s - "$APP"
echo "installed $APP"
