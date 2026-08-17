#!/bin/sh
set -eu

version="${1:?usage: package-macos-app.sh <version> <arm64|x64> [output-directory]}"
architecture="${2:?usage: package-macos-app.sh <version> <arm64|x64> [output-directory]}"
output_root="${3:-release}"
project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
case "$architecture" in
  arm64)
    target="aarch64-apple-darwin"
    expected_architecture="arm64"
    ;;
  x64)
    target="x86_64-apple-darwin"
    expected_architecture="x86_64"
    ;;
  *) echo "invalid architecture: $architecture" >&2; exit 2 ;;
esac
app="$project_root/src-tauri/target/$target/release/bundle/macos/BnetSwitchLite.app"
artifact="$project_root/$output_root/BnetSwitchLite-$version-macos-$architecture.zip"
require_notarization="${BNETSWITCHLITE_REQUIRE_NOTARIZATION:-0}"

case "$version" in
  *[!0-9A-Za-z.-]*|'') echo "invalid version: $version" >&2; exit 2 ;;
esac

test -d "$app" || { echo "signed app not found: $app" >&2; exit 1; }
codesign --verify --deep --strict --verbose=2 "$app"

if [ "$require_notarization" = "1" ]; then
  xcrun stapler validate "$app"
fi

architectures=$(lipo -archs "$app/Contents/MacOS/BnetSwitchLite")
test "$architectures" = "$expected_architecture" || {
  echo "expected $expected_architecture app, found: $architectures" >&2
  exit 1
}

mkdir -p "$project_root/$output_root"
rm -f -- "$artifact"
ditto -c -k --sequesterRsrc --keepParent "$app" "$artifact"
unzip -t "$artifact" >/dev/null
shasum -a 256 "$artifact"
printf '%s\n' "$artifact"
