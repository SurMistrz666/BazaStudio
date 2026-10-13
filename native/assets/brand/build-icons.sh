#!/usr/bin/env bash
# Regenerates the committed icon binaries from the SVG masters in this directory.
#
#   baza-<app>-macos.svg           macOS artwork, 64 px and larger (824 px plate on a 1024 canvas)
#   baza-<app>-macos-small.svg     macOS artwork hinted for the 32 px frame (16 pt on Retina)
#   baza-<app>-windows.svg         Windows artwork, 48 px and larger (plate fills the canvas)
#   baza-<app>-windows-small.svg   Windows artwork hinted for 16, 24 and 32 px
#
# Outputs, next to the sources:
#
#   baza-<app>.icns                macOS bundle icon (16@2x, 32@2x, 128, 256, 512 and their @2x)
#   baza-<app>.ico                 Windows executable icon (16, 24, 32, 48, 64, 128, 256)
#   baza-<app>-dock-512.png        runtime Dock icon for unbundled macOS runs (a packaged
#                                  application keeps the icon of its bundle)
#   baza-<app>-window-256.png      runtime window and taskbar icon elsewhere (small artwork,
#                                  because the toolkit scales it down to 16..48 px)
#
# Needs macOS (iconutil), Google Chrome as the SVG rasteriser and ImageMagick 7 (magick).
# Every size is box-filtered from an integer multiple, so edges drawn on the pixel grid
# of a master stay on the pixel grid of the output.
set -euo pipefail
umask 022
[[ "$(uname -s)" == Darwin ]] || { printf '%s\n' 'Generate icons on macOS: iconutil is required.' >&2; exit 2; }
brand="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
chrome="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
[[ -x "$chrome" ]] || { printf '%s\n' 'Google Chrome is missing; set CHROME to its executable.' >&2; exit 2; }
command -v magick >/dev/null || { printf '%s\n' 'ImageMagick 7 (magick) is missing.' >&2; exit 2; }
[[ -x /usr/bin/iconutil ]] || { printf '%s\n' 'iconutil is missing.' >&2; exit 2; }
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

# Rasterise an SVG to a square transparent PNG: render <svg> <png> <pixels>.
render() {
  cp -- "$1" "$work/art.svg"
  printf '<!doctype html><style>html,body{margin:0;background:transparent;overflow:hidden}img{display:block;width:%spx;height:%spx}</style><img src="art.svg">' "$3" "$3" > "$work/page.html"
  "$chrome" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
    --default-background-color=00000000 --window-size="$3,$3" \
    --screenshot="$2" "file://$work/page.html" >/dev/null 2>&1
  [[ "$(magick identify -format '%wx%h' "$2")" == "$3x$3" ]] || { printf '%s\n' "Rasterising $1 failed." >&2; exit 1; }
}

# Downsample by an integer factor: shrink <master png> <png> <pixels>.
shrink() {
  magick "$1" -filter Box -resize "$3x$3" -strip \
    -define png:exclude-chunks=date,time -define png:compression-level=9 "PNG32:$2"
}

for app in baza-studio; do
  for master in macos macos-small windows windows-small; do
    [[ -f "$brand/$app-$master.svg" ]] || { printf '%s\n' "Missing $app-$master.svg." >&2; exit 2; }
  done
  # 3072 = 3 * 1024 = 64 * 48 and 1536 = 96 * 16 = 64 * 24 = 48 * 32.
  render "$brand/$app-macos.svg" "$work/macos.png" 3072
  render "$brand/$app-macos-small.svg" "$work/macos-small.png" 1536
  render "$brand/$app-windows.svg" "$work/windows.png" 3072
  render "$brand/$app-windows-small.svg" "$work/windows-small.png" 1536

  # The 1x frames icon_16x16 and icon_32x32 are left out on purpose. macOS 26 and later
  # re-mask frames of 64 px and up to the current plate shape and show the 16@2x frame as
  # drawn, but put those two 1x frames on a grey backing plate. Without them every system
  # scales a larger frame down instead, which costs nothing visible on older releases.
  iconset="$work/$app.iconset"
  mkdir -- "$iconset"
  shrink "$work/macos-small.png" "$iconset/icon_16x16@2x.png" 32
  shrink "$work/macos.png" "$iconset/icon_32x32@2x.png" 64
  shrink "$work/macos.png" "$iconset/icon_128x128.png" 128
  shrink "$work/macos.png" "$iconset/icon_128x128@2x.png" 256
  shrink "$work/macos.png" "$iconset/icon_256x256.png" 256
  shrink "$work/macos.png" "$iconset/icon_256x256@2x.png" 512
  shrink "$work/macos.png" "$iconset/icon_512x512.png" 512
  shrink "$work/macos.png" "$iconset/icon_512x512@2x.png" 1024
  /usr/bin/iconutil --convert icns --output "$brand/$app.icns" "$iconset"

  frames=()
  for size in 16 24 32; do
    shrink "$work/windows-small.png" "$work/ico-$size.png" "$size"
    frames+=("$work/ico-$size.png")
  done
  for size in 48 64 128 256; do
    shrink "$work/windows.png" "$work/ico-$size.png" "$size"
    frames+=("$work/ico-$size.png")
  done
  # Conventional layout: 32-bit bitmaps below 256 px, PNG for the 256 px frame.
  magick "${frames[@]}" -define icon:png-compression-size=256 "$brand/$app.ico"

  cp -- "$iconset/icon_256x256@2x.png" "$brand/$app-dock-512.png"
  shrink "$work/windows-small.png" "$brand/$app-window-256.png" 256
  rm -rf -- "$iconset" "$work"/ico-*.png
  printf '%s\n' "$app: icns, ico, dock-512.png, window-256.png"
done
