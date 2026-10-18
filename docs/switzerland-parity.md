# OpenMapTiles Switzerland: Java vs Rust output parity

Checked 2026-10-18 on the outputs of the last end-to-end trial of the benchmark run in
[`bench/RESULTS.md`](../bench/RESULTS.md) (Rust `627d901`, Java planetiler `d88cf89`, default threads on both sides,
`switzerland-latest.osm.pbf`, same cached sources). The comparison was made after timing and was not timed. This
run includes the JTS offset-curve / `CascadedPolygonUnion` port used by `merge_nearby_polygons` ("Fix B" in
[`analysis-merge-vertices.md`](analysis-merge-vertices.md)) and the JTS `Polygonizer` used for OpenMapTiles boundary
country polygons.

## How

- `target/release/planetiler compare java.mbtiles rust.mbtiles` is the check (`util::compare_archives`, port of Java
  `CompareArchives` with a normalized feature comparison). Tiles with equal bytes are skipped; the others are decoded
  in parallel, layers are paired by name and each layer is compared as a multiset of features keyed on `(geometry
  type, normalized geometry, tags[, id])`: tags compared as a map, geometry in absolute tile coordinates with the
  points of a multipoint and the lines of a multilinestring sorted, polygon rings rotated to start at their smallest
  vertex (orientation kept, it marks holes), holes sorted within their polygon and polygons sorted. It prints, per
  layer, the features only in archive 1 / only in archive 2 (total and by zoom), how many of those pairs differ only
  in feature ID, and a few examples. `--ignore-ids` drops feature IDs from the key, `--examples=N` sets the number
  of examples per layer, `--verbose` logs every differing feature. It replaces the Python scripts previously used
  for this check (`/tmp/pt_scripts/mvtnorm{,2}.py`, not in the repo) and gives the same counts in ~1 s for
  Switzerland (vs ~1.5-3 min and ~12 GB of RAM per script).
- Per-layer feature counts (the table below) come from `/tmp/pt_scripts/counts.py`: `compare` skips identical tiles
  without decoding them, so it doesn't count features.
- Inputs: `bench/out/20261018-054434/e2e_{switzerland,monaco}/{java,rust}.mbtiles` (kept by `bench/run_all.sh` for
  the last trial).

## Results

**Monaco:** identical to Java in every layer: same 23 tiles, same per-layer feature counts, and the normalized
comparison finds no differing feature, with or without feature IDs.

**Switzerland tiles:** 23,922 on both sides, identical per zoom (z0-6: 1 each, z7 4, z8 12, z9 35, z10 123, z11 385,
z12 1,280, z13 4,623, z14 17,453), no tile present on only one side. Archive size 365,977,600 bytes (Java) vs
362,565,632 (Rust); tile data 348.9 MB vs 345.6 MB.

**Feature count per layer** (all tiles, all zooms): identical in all 16 layers.

| Layer | Java = Rust |
|-------|-------------|
| aerodrome_label | 709 |
| aeroway | 8,723 |
| boundary | 25,402 |
| building | 39,652 |
| housenumber | 825,203 |
| landcover | 792,239 |
| landuse | 118,588 |
| mountain_peak | 183,755 |
| park | 16,058 |
| place | 445,358 |
| poi | 1,448,156 |
| transportation | 747,920 |
| transportation_name | 394,438 |
| water | 117,266 |
| water_name | 26,313 |
| waterway | 410,288 |

The extra Rust building at the 4 px² merge area threshold that the previous comparison found (tile 13/4281/2891) is
gone with the JTS buffer port.

**Differing features** (`planetiler compare`; a feature differs if no feature with the same layer, type, tags and
normalized geometry exists in the other side's tile; counts are Java-only / Rust-only):

| Layer | Now (`627d901`) | Previous pass (`cb984fd`) | Where | Cause |
|-------|-----------------|---------------------------|-------|-------|
| building | 578 / 578 | 2,387 / 2,388 | all z13 | merged buildings (`merge_nearby_polygons` buffer/unbuffer): remaining vertex-level differences vs JTS |
| landuse | 35 / 35 | 404 / 404 | z6-z12 (+1 at z14) | same buffer-merge vertex difference |
| landcover | 17 / 17 | 21 / 21 | z11-z14 | same buffer-merge vertex difference |
| transportation | 10 / 10 | 10 / 10 | z9-z14 | line merging picks a different continuation at a junction / 1-unit vertex shifts |
| housenumber | 1,730 / 1,730 (IDs only) | 1,730 | | merged multipoints keep a different feature's ID (multi-threaded input order); geometry and tags identical |
| boundary | 0 | 0 | | identical, now via the `Polygonizer` port for `adm0_l`/`adm0_r` |
| all other layers | 0 | 0 | | identical, including feature IDs |

These numbers are the same as the one-off check made right after the Fix B port, so the later commits changed
nothing in the output.

**`planetiler compare`** output on these files (the differing-feature numbers above): 1,697 of 23,922 tiles differ
(635 with `--ignore-ids`); the other 22,032 have different bytes but equivalent normalized contents (key/value table
order, ring start point, ring / feature order). Per layer it reports `building` 578 / 578 (z13), `landuse` 35 / 35,
`landcover` 17 / 17, `transportation` 10 / 10 and `housenumber` 1,730 / 1,730, all 1,730 in its "id only" column (gone
with `--ignore-ids`). On Monaco it reports 0 differing features, with or without IDs (all 23 tiles differ in bytes
only). The command exits 1 when a tile differs, and by default also on archive-level differences such as the
metadata (build time, version and commit differ between Java and Rust); `--strict=false` ignores those.

## Reproduce

```sh
bench/run_all.sh e2e_switzerland            # or TRIALS=1; keeps the last trial's outputs
D=bench/out/<run>/e2e_switzerland
target/release/planetiler compare $D/java.mbtiles $D/rust.mbtiles               # including IDs
target/release/planetiler compare --ignore-ids $D/java.mbtiles $D/rust.mbtiles  # ignoring IDs
python3 -I /tmp/pt_scripts/counts.py $D/java.mbtiles $D/rust.mbtiles           # feature counts per layer
```

`compare` takes ~1 s for Switzerland (15 cores); `counts.py` needs ~12 GB of RAM and ~1.5 minutes.
