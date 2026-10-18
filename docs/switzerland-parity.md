# OpenMapTiles Switzerland: Java vs Rust output parity

Checked 2026-10-18 on the outputs of the last end-to-end trial of the benchmark run in
[`bench/RESULTS.md`](../bench/RESULTS.md) (Rust `627d901`, Java planetiler `d88cf89`, default threads on both sides,
`switzerland-latest.osm.pbf`, same cached sources). The comparison was made after timing and was not timed. This
run includes the JTS offset-curve / `CascadedPolygonUnion` port used by `merge_nearby_polygons` ("Fix B" in
[`analysis-merge-vertices.md`](analysis-merge-vertices.md)) and the JTS `Polygonizer` used for OpenMapTiles boundary
country polygons.

## How

- `target/release/planetiler compare java.mbtiles rust.mbtiles` (port of Java `CompareArchives`). It pairs layers by
  name and decodes tiles, reporting tiles whose bytes differ but whose decoded contents are equivalent separately
  (key/value table order no longer counts). It still compares geometry commands strictly, so a polygon ring with a
  different start point or a different ring / feature order counts as a difference.
- Normalized comparison with the scripts used for the earlier orchestrator comparison (`/tmp/pt_scripts`, not in the
  repo): `mvt.py` decodes every tile; `counts.py` counts features per layer; `mvtnorm2.py` compares each tile as a
  multiset of `(layer, geometry type, sorted tags, normalized geometry)` (polygon rings compared ignoring start point
  and ring order), ignoring feature IDs; `mvtnorm.py` is the same but includes feature IDs.
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

**Differing features** (normalized comparison; a feature differs if no feature with the same layer, type, tags and
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

**`planetiler compare`** reports 20,735 of 23,922 tiles as different and 2,994 more as "different bytes but
equivalent decoded contents" (the previous version reported 23,729, nearly all from key/value table order). The
remaining reports are ring start point / ring order / feature order differences that the normalized comparison
treats as equal, plus the real differences above: `landcover` 65,778 polygon commands, `building` 4,551, `landuse`
4,470, `water` 1,643, `park` 602, `transportation` 180 polygon / 10 linestring commands, `aeroway` 33, and
`housenumber` feature IDs in 1,062 / feature order in 8,220 tiles. On Monaco it reports 20 of 23 tiles (polygon ring
geometry in `building`, `water` and `landcover`, `housenumber` feature order) although the decoded contents are
equivalent up to ring start point and order. So `compare` is still not a pass/fail signal for this port; the
normalized comparison is.

## Reproduce

```sh
bench/run_all.sh e2e_switzerland            # or TRIALS=1; keeps the last trial's outputs
D=bench/out/<run>/e2e_switzerland
target/release/planetiler compare $D/java.mbtiles $D/rust.mbtiles
python3 -I /tmp/pt_scripts/counts.py $D/java.mbtiles $D/rust.mbtiles
python3 -I /tmp/pt_scripts/mvtnorm2.py $D/java.mbtiles $D/rust.mbtiles   # ignoring IDs
python3 -I /tmp/pt_scripts/mvtnorm.py $D/java.mbtiles $D/rust.mbtiles 3  # including IDs
```

Each Python script needs ~12 GB of RAM and ~1.5-3 minutes for Switzerland.
