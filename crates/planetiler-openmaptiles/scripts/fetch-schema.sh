#!/usr/bin/env bash
# usage: fetch.sh <ref> <dest>
set -euo pipefail
REF="$1"
D="$2"
B="https://raw.githubusercontent.com/openmaptiles/openmaptiles/$REF"
mkdir -p "$D"
curl -sSf "$B/openmaptiles.yaml" -o "$D/openmaptiles.yaml"
for f in $(grep -E '^\s+- layers/' "$D/openmaptiles.yaml" | sed 's/.*- //'); do
  mkdir -p "$D/$(dirname "$f")"
  curl -sSf "$B/$f" -o "$D/$f"
  for m in $(grep -E '^\s+mapping_file:' "$D/$f" | sed 's/.*mapping_file:\s*//'); do
    p="$(dirname "$f")/$m"
    mkdir -p "$D/$(dirname "$p")"
    curl -sSf "$B/$p" -o "$D/$p"
    echo "$p"
  done
done
echo "$REF" > "$D/REF"
