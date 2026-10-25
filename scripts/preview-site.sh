#!/bin/sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

cp -R "$root/docs/site/." "$tmp/"
mkdir -p "$tmp/assets/images"
cp "$root"/images/*.png "$tmp/assets/images/"

cd "$tmp"
echo "Serving $tmp at http://localhost:8000"
python3 -m http.server 8000
