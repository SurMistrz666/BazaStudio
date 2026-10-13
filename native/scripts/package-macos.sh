#!/usr/bin/env bash
set -euo pipefail
umask 077
[[ "$(uname -s)" == Darwin ]] || { printf '%s\n' 'Package on a native macOS runner.' >&2; exit 2; }
native_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
output="${1:?Pass the artifact directory}"
mkdir -p -- "$output"
output="$(cd -- "$output" && pwd -P)"
target="${CARGO_TARGET_DIR:-$native_root/target}/release"
architecture="$(uname -m)"
[[ "$architecture" == arm64 || "$architecture" == x86_64 ]] || exit 2
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$native_root/Cargo.toml" | head -n 1)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || exit 2
bundle_version="${version%%-*}"
repository_root="$(cd -- "$native_root/.." && pwd -P)"
for document in "$repository_root/LICENSE" "$native_root/THIRD-PARTY-NOTICES.txt" "$native_root/THIRD-PARTY-INVENTORY.json"; do
  [[ -f "$document" && ! -L "$document" ]] || { printf '%s\n' 'Required package notice document is missing.' >&2; exit 2; }
done
license_directory="$native_root/third-party-licenses"
[[ -d "$license_directory" && ! -L "$license_directory" && -n "$(find "$license_directory" -type f -print -quit)" && -z "$(find "$license_directory" -type l -print -quit)" ]] || { printf '%s\n' 'Required ordinary third-party license assets are missing.' >&2; exit 2; }
icon_directory="$native_root/assets/brand"
for icon in baza-studio.icns; do
  [[ -f "$icon_directory/$icon" && ! -L "$icon_directory/$icon" ]] || { printf '%s\n' 'Required application icon is missing.' >&2; exit 2; }
done
for program in BazaStudio; do
  case "$program" in
    BazaStudio) display_name='Baza Studio'; icon_source="$icon_directory/baza-studio.icns" ;;
    *) exit 2 ;;
  esac
  source_binary="$target/$program"
  [[ -f "$source_binary" && ! -L "$source_binary" ]] || { printf '%s\n' 'Build native release binaries first.' >&2; exit 2; }
  package="$output/$program-$version-macOS-$architecture"
  [[ ! -e "$package.app" && ! -e "$package.dmg" ]] || { printf '%s\n' 'Package destination exists.' >&2; exit 2; }
  app="$package.app"
  mkdir -p -- "$app/Contents/MacOS" "$app/Contents/Resources"
  cp -- "$source_binary" "$app/Contents/MacOS/$program"
  cp -- "$repository_root/LICENSE" "$native_root/THIRD-PARTY-NOTICES.txt" "$native_root/THIRD-PARTY-INVENTORY.json" "$app/Contents/Resources/"
  cp -R -- "$license_directory" "$app/Contents/Resources/third-party-licenses"
  cp -- "$icon_source" "$app/Contents/Resources/AppIcon.icns"
  product_id="$(printf '%s' "$program" | tr '[:upper:]' '[:lower:]')"
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>$program</string>
<key>CFBundleDisplayName</key><string>$display_name</string>
<key>CFBundleIdentifier</key><string>pl.bazastudio.$product_id</string>
<key>CFBundleExecutable</key><string>$program</string>
<key>CFBundleIconFile</key><string>AppIcon</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$bundle_version</string>
<key>CFBundleVersion</key><string>$bundle_version</string>
<key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
<key>NSHumanReadableCopyright</key><string>Copyright © 2026 SurMistrz666. GNU GPL version 3 or later.</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
  /usr/bin/plutil -lint "$app/Contents/Info.plist"
  # Ad-hoc code signature is an integrity check, not publisher signing/notarization.
  /usr/bin/codesign --force --sign - "$app"
  /usr/bin/codesign --verify --deep --strict "$app"
  for mode in self-test native-ui-self-test; do
    report="$output/$program-$mode.json"
    "$app/Contents/MacOS/$program" "--$mode" --report "$report" &
    qa_pid=$!
    for ((attempt=0; attempt<120; attempt++)); do
      kill -0 "$qa_pid" 2>/dev/null || break
      sleep 1
    done
    if kill -0 "$qa_pid" 2>/dev/null; then
      kill "$qa_pid"; wait "$qa_pid" || true
      printf '%s\n' 'Native packaged acceptance timed out.' >&2; exit 1
    fi
    wait "$qa_pid"
    field=passed
    [[ "$mode" == native-ui-self-test ]] && field=renderer_screenshot
    result="$(/usr/bin/plutil -extract "$field" raw -o - "$report")"
    [[ "$result" == true || "$result" == 1 ]] || { printf '%s\n' 'Native report refused publication.' >&2; exit 1; }
    if [[ "$mode" == native-ui-self-test ]]; then
      result="$(/usr/bin/plutil -extract vault_opened raw -o - "$report")"
      [[ "$result" == true || "$result" == 1 ]] || exit 1
    fi
  done
  /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$app" "$package.app.zip"
  stage="$output/$program-dmg-stage"
  [[ ! -e "$stage" ]] || exit 2
  mkdir -- "$stage"
  /usr/bin/ditto "$app" "$stage/$program.app"
  ln -s /Applications "$stage/Applications"
  /usr/bin/hdiutil create -fs HFS+ -format UDZO -volname "$program" -srcfolder "$stage" "$package.dmg"
  /usr/bin/hdiutil verify "$package.dmg"
  # File names only: the list is published next to the packages.
  (cd -- "$output" && /usr/bin/shasum -a 256 "${package##*/}.dmg" "${package##*/}.app.zip") > "$package.sha256"
done
