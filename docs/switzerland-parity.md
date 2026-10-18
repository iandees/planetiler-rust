# OpenMapTiles Switzerland: Java vs Rust output parity

Checked 2026-10-18 on the outputs of the last end-to-end trial of the benchmark run in
[`bench/RESULTS.md`](../bench/RESULTS.md) (Rust `cb984fd`, Java planetiler `d88cf89`, default threads on both sides,
`switzerland-latest.osm.pbf`, same cached sources). The comparison was made after timing and was not timed.

## How

- `target/release/planetiler compare java.mbtiles rust.mbtiles` (port of Java `CompareArchives`; strict: compares
  encoded tiles, including key/value table order and feature IDs).
- Normalized comparison with the scripts used for the earlier orchestrator comparison (`/tmp/pt_scripts`, not in the
  repo): `mvt.py` decodes every tile; `counts.py` counts features per layer; `mvtnorm2.py` compares each tile as a
  multiset of `(layer, geometry type, sorted tags, normalized geometry)` (polygon rings compared ignoring start point
  and ring order), ignoring feature IDs; `mvtnorm.py` is the same but includes feature IDs.
- Inputs: `bench/out/20261018-022316/e2e_switzerland/{java,rust}.mbtiles` (kept by `bench/run_all.sh` for the last
  trial).

## Results

**Tiles:** 23,922 on both sides, identical per zoom (z0-6: 1 each, z7 4, z8 12, z9 35, z10 123, z11 385,
z12 1,280, z13 4,623, z14 17,453), no tile present on only one side. The previous comparison had 28 tiles that only
Java wrote (empty `mountain_peak` tiles); that is fixed. Archive size 365,977,600 bytes (Java) vs 362,479,616 (Rust);
tile data 348.9 MB vs 345.5 MB.

**Feature count per layer** (all tiles, all zooms):

| Layer | Java | Rust |
|-------|------|------|
| aerodrome_label | 709 | 709 |
| aeroway | 8,723 | 8,723 |
| boundary | 25,402 | 25,402 |
| building | 39,652 | **39,653** |
| housenumber | 825,203 | 825,203 |
| landcover | 792,239 | 792,239 |
| landuse | 118,588 | 118,588 |
| mountain_peak | 183,755 | 183,755 |
| park | 16,058 | 16,058 |
| place | 445,358 | 445,358 |
| poi | 1,448,156 | 1,448,156 |
| transportation | 747,920 | 747,920 |
| transportation_name | 394,438 | 394,438 |
| water | 117,266 | 117,266 |
| water_name | 26,313 | 26,313 |
| waterway | 410,288 | 410,288 |

**Differing features** (normalized comparison; a feature differs if no feature with the same layer, type, tags and
normalized geometry exists in the other side's tile; counts are Java-only / Rust-only):

| Layer | Now | Previous comparison | Where | Cause |
|-------|-----|---------------------|-------|-------|
| building | 2,387 / 2,388 | 2,791 | all z13 | merged buildings (`merge_nearby_polygons` buffer/unbuffer): vertex-level differences between `geo` buffer output and JTS's offset curves |
| landuse | 404 / 404 | 725 | z6-z12 (+1 at z14) | same buffer-merge vertex difference |
| landcover | 21 / 21 | 21 | z7-z14 | same buffer-merge vertex difference |
| transportation | 10 / 10 | 10 | z9-z14 | line merging picks a different continuation at a junction / 1-unit vertex shifts |
| housenumber | 1,730 / 1,730 (IDs only) | 1,730 | | merged multipoints keep a different feature's ID (multi-threaded input order); geometry and tags identical |
| all other layers | 0 | 0 | | identical, including feature IDs |

The near-collinear vertex removal on merged polygons (`feature_merge`, "Fix A" in
[`analysis-merge-vertices.md`](analysis-merge-vertices.md)) cut the building diffs by 14% and landuse by 44%; exact
parity still needs a port of JTS's offset-curve builder.

**The one extra building** is in tile 13/4281/2891 (only Rust has a `building` layer there): way 38316928 (feature ID
383169280), an 11-vertex merged polygon whose output area is 4.002 px², just over `merge_nearby_polygons`' 4 px² min
area. Java's buffer/unbuffer result for the same input was just under the threshold, so it dropped it. Same root cause
as the building row above, at the area filter instead of in the vertices.

**`planetiler compare`** reports 23,729 of 23,922 tiles as different, nearly all because of key/value table *order*
inside layers (e.g. `landcover` 22,024 tiles, `place` 19,360, `waterway` 19,326), which does not change decoded
content. Besides that it reports the geometry differences above (`building` 4,649 tiles with differing polygon
commands, `landcover` 65,767 commands, `landuse` 4,557, `transportation` 10 line / 180 polygon commands, `water` 1,643,
`park` 602, `aeroway` 33) and `housenumber` feature IDs in 9,282 tiles. The `features count: 1` entries it prints for
`landcover`, `mountain_peak`, `place`, `poi`, `transportation`, `transportation_name`, `water` and `waterway` all come
from tile 13/4281/2891: the tool pairs layers by position, so the extra `building` layer shifts every later layer by
one. The decoded per-tile layer counts in that tile are otherwise identical.

Note that `compare` counts polygon geometry command differences that the normalized comparison treats as equal
(different ring start point or ring order), which is why its `landcover`/`water`/`park`/`aeroway` numbers are larger
than the normalized ones (0 for `water`, `park`, `aeroway`).

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
