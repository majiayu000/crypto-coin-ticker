#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
app_name="CryptoTicker"
bundle_id="com.starlight.cryptoticker"
app_dir="$root/dist/${app_name}.app"
entitlements="$root/Resources/CryptoTicker.entitlements"
identity="${TICKER_SIGNING_IDENTITY:-${APPLE_SIGNING_IDENTITY:--}}"

cd "$root"
cargo build --release
binary="$root/target/release/okk"
[[ -x "$binary" ]] || { echo "missing binary: $binary" >&2; exit 1; }
[[ -f "$entitlements" ]] || { echo "missing entitlements: $entitlements" >&2; exit 1; }

if [[ "${TICKER_REQUIRE_DEVELOPER_ID:-}" == "1" ]]; then
  if [[ "$identity" == "-" || "$identity" != Developer\ ID\ Application:* ]]; then
    echo "TICKER_REQUIRE_DEVELOPER_ID=1 needs APPLE_SIGNING_IDENTITY to be a Developer ID Application identity" >&2
    exit 1
  fi
fi

rm -rf "$app_dir"
mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
install -m 755 "$binary" "$app_dir/Contents/MacOS/$app_name"
if [[ -f "$root/icons/icon.png" ]]; then
  install -m 644 "$root/icons/icon.png" "$app_dir/Contents/Resources/icon.png"
fi

cat > "$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>$app_name</string>
  <key>CFBundleIdentifier</key>
  <string>$bundle_id</string>
  <key>CFBundleName</key>
  <string>Crypto Coin Ticker</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.2.0</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

if [[ "$identity" == "-" ]]; then
  timestamp_args=(--timestamp=none)
else
  timestamp_args=(--timestamp)
fi

codesign --force --sign "$identity" --identifier "$bundle_id" \
  --options runtime --entitlements "$entitlements" "${timestamp_args[@]}" \
  "$app_dir/Contents/MacOS/$app_name"
codesign --force --sign "$identity" --identifier "$bundle_id" \
  --options runtime --entitlements "$entitlements" "${timestamp_args[@]}" \
  "$app_dir"
codesign --verify --deep --strict "$app_dir"
echo "$app_dir"
