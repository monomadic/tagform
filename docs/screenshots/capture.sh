#!/bin/zsh
# Regenerates the README screenshots.
#
# Drives target/release/tagform through a pseudo-terminal (shot.py, which
# answers the terminal-capability queries the app makes on startup), snapshots
# the screen grid with pyte, and renders it to SVG then PNG (render.py). The
# thumbnail cells are painted with the real frame, since that is what a
# graphics-capable terminal shows there.
#
# Needs: cargo, ffmpeg, rsvg-convert, magick, uv (for a throwaway venv with
# pyte + pillow). Sample media is generated in a temp dir and thrown away.
set -e
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cargo build --release --manifest-path "$root/Cargo.toml" >/dev/null
T="$root/target/release/tagform"

uv venv -q "$work/venv" && uv pip install -q --python "$work/venv/bin/python" pyte pillow
P="$work/venv/bin/python"

cd "$work" && mkdir media
mk() { out="$1"; shift; ffmpeg -loglevel error -y -f lavfi -i "$SRC" -f lavfi -i "sine=frequency=440" -t 3 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -movflags use_metadata_tags "$@" "media/$out"; }
A="media/Kelsey Vance (Fabric London) - Neon Skyline.mp4"
B="media/Kelsey Vance (Fabric London) - Afterglow.mp4"
C="media/Kelsey Vance (Fabric London) - Soundcheck.mp4"
SRC="mandelbrot=size=640x360:rate=25" mk "${A#media/}" -metadata title="Neon Skyline (Live at Fabric)" \
  -metadata artist="Kelsey Vance" -metadata actors="Kelsey Vance, Marco Diaz" -metadata channel="Fabric London" \
  -metadata category="Live Visual" -metadata variant="Original" -metadata rating=4 \
  -metadata keywords="#synthwave #live #london #laser" -metadata genre="Electronic" -metadata date="2025-11-02" \
  -metadata purl="https://www.youtube.com/watch?v=dQw4w9WgXcQ" \
  -metadata description="Full set recorded from the balcony. Second half has the laser section."
SRC="testsrc2=size=640x360:rate=25" mk "${B#media/}" -metadata title="Afterglow" -metadata artist="Kelsey Vance" \
  -metadata actors="Kelsey Vance" -metadata channel="Fabric London" -metadata category="Live Visual" \
  -metadata variant="Clip" -metadata rating=3 -metadata keywords="#synthwave #live #encore" \
  -metadata genre="Electronic" -metadata date="2025-11-02" -metadata purl="https://www.youtube.com/watch?v=abc123xyz00"
SRC="life=size=640x360:rate=25" mk "${C#media/}" -metadata title="Soundcheck" -metadata artist="Kelsey Vance" \
  -metadata channel="Fabric London" -metadata category="Live Visual" -metadata rating=2 \
  -metadata keywords="#soundcheck #london" -metadata date="2025-11-01"
ffmpeg -loglevel error -y -ss 1 -i "$A" -frames:v 1 thumb.png

export COLS=100 ROWS=32
$P "$here/shot.py" form.json '[]' "$T" "$A"
$P "$here/shot.py" edit.json '["jj","\r",""," (Full Set)"]' "$T" "$A"
$P "$here/shot.py" bulk.json '[]' "$T" "$A" "$B" "$C"
$P "$here/shot.py" merge.json '["jjjjjjjjj","m"]' "$T" "$A" "$B" "$C"
COLS=120 ROWS=20 $P "$here/shot.py" plan.json '["jjjjjjjjj","m","w"]' "$T" "$A" "$B" "$C"
$P "$here/shot.py" help.json '["?"]' "$T" "$A"
for th in gruvbox nord amber c64; do $P "$here/shot.py" theme-$th.json '[]' "$T" --theme=$th "$A"; done

for f in form edit bulk merge plan help theme-gruvbox theme-nord theme-amber theme-c64; do
  $P "$here/render.py" $f.json $f.svg thumb.png && rsvg-convert -z 2 $f.svg -o $f.png
done
magick \( theme-gruvbox.png theme-nord.png +append \) \( theme-amber.png theme-c64.png +append \) -append themes.png
for f in form edit bulk merge plan help themes; do
  magick $f.png -colors 255 -define png:compression-level=9 "$here/$f.png"
done
echo "wrote $here/{form,edit,bulk,merge,plan,help,themes}.png"
