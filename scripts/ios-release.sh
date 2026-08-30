#!/usr/bin/env bash
# Archives and uploads a TestFlight/App Store build with no Xcode GUI interaction,
# using the App Store Connect API key at ~/.appstoreconnect/private_keys instead of
# an interactively-signed-in Apple ID (which xcodebuild can't drive headlessly).
#
# Usage: scripts/ios-release.sh <build-number>
set -euo pipefail

BUILD_NUMBER="${1:?usage: ios-release.sh <build-number>}"
API_KEY_ID="5VU72NRAK7"
API_ISSUER_ID="5c7d20ab-6029-4e0b-b5d3-19687c20be71"
API_KEY_PATH="$HOME/.appstoreconnect/private_keys/AuthKey_${API_KEY_ID}.p8"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APPLE_DIR="$ROOT_DIR/src-tauri/gen/apple"
ARCHIVE_PATH="$APPLE_DIR/build/mountain-view_iOS.xcarchive"
EXPORT_PATH="$APPLE_DIR/build/export"

cd "$ROOT_DIR"
pnpm tauri ios build --archive-only --build-number "$BUILD_NUMBER"

rm -rf "$EXPORT_PATH"
xcodebuild -exportArchive \
  -archivePath "$ARCHIVE_PATH" \
  -exportPath "$EXPORT_PATH" \
  -exportOptionsPlist "$APPLE_DIR/ExportOptions.plist" \
  -allowProvisioningUpdates \
  -authenticationKeyPath "$API_KEY_PATH" \
  -authenticationKeyID "$API_KEY_ID" \
  -authenticationKeyIssuerID "$API_ISSUER_ID"

echo "Uploaded build 0.1.0.${BUILD_NUMBER} to App Store Connect."
