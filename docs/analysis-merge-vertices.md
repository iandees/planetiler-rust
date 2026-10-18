# Analysis: extra vertices in merged polygons, and missing empty tiles

Read-only analysis (no builds) against geo 0.33.1 (`Buffer` uses i_overlay 4.5.2 / i_float 1.16) and JTS 1.20.

## 1. Merged polygons have ~50% more vertices than Java

Path: `merge_nearby_polygons_with` -> `buffer_union_unbuffer` (`crates/planetiler-core/src/feature_merge.rs:308-389`,
`:512-565`) -> `buffer_geometry` (`feature_merge.rs:577-603`) ->
`geo::Buffer::buffer_with_style(LineJoin::Miter(2*asin(1/5)))` (i_overlay `OutlineOffset::outline`). After the
unbuffer only `snap_and_fix_polygon` runs (`geo/geo_utils.rs:325-370`), same as Java; no DP/VW on either side.

**Root cause:** i_overlay's mitre join emits 3 vertices per outside corner where JTS emits 1.

- JTS `OffsetSegmentGenerator.addOutsideTurn` -> `addMitreJoin` adds just the mitre point (2 bevel points when
  limited; nothing for collinear).
- i_overlay (`mesh/outline/builder.rs:127-150`, `builder_join.rs:141-150`) emits `ia (= s0.b_top) -> ic (peak) ->
  ib (= s1.a_top)`; `ia`/`ib` are collinear with the adjacent offset segments but rounded independently to the integer
  grid, so `simplify_contour` (`offset.rs:320-334`, exact integer collinearity only) keeps them on non-axis-aligned
  edges. Axis-aligned unit tests hide this.
- Through buffer -> union -> unbuffer, convex corners collapse back, but every concave corner (original concavities
  and every bridged gap) is an outside turn for the `-d` pass, leaving 2 extra points on the true edges ~`d` from the
  corner (8 grid units for buildings, which survive 1/16 px rounding). +2 vertices per concave corner ≈ +50%, area
  unchanged.
- Minor: no `BufferInputLineSimplifier` port; limited-mitre bevel placement differs; vertex snapping tolerances differ
  (all below the tile grid).

**Status: Fix A implemented** (`feature_merge.rs` `remove_near_collinear_ring`, removal is smallest deviation first so
very shallow corners keep one of their three points). The rotated-L / rotated-rectangle test
(`merge_rotated_polygons_match_java`) now matches Java's `mergeNearbyPolygons` output exactly at 0/17/30/45/72
degrees (the 30-degree L is 8 points like Java; it was 11; Java itself keeps 1-2 extra points where rounding the input
to the tile grid leaves bridged edges slightly non-collinear). Monaco OpenMapTiles: the 4 merged z13 `building`
features have 3610 points vs Java's 3666 (5823 before), and 163 of 211 rings are identical (54 before). They still
differ: about 90 vertices Java keeps that i_overlay's offset curve cuts across (shallow input corners within a fraction
of a pixel of the line), and about 35 one-unit shifts. Those need Fix B.

**Fix A (recommended, small):** remove near-collinear vertices from `geo::Buffer` output in `buffer_geometry`
(`feature_merge.rs:595-602`), for both `+d` and `-d`. Drop vertex b between a and c when
`|cross(b-a, c-a)| / |c-a| <= eps` and `dot(b-a, c-b) > 0` (keep spikes); wrap the closing vertex; drop rings < 4
points. Use `eps ≈ H * 2^-25` with `H = 0.5 * max(bbox w, h) + 6|d|` (≈16-32 i_overlay grid units; far below the
≥1e-5 px offset of any real vertex on the 1/16 grid). JTS buffers never emit collinear vertices, so this matches.
Do not put it in `snap_and_fix` (Java keeps collinear points there).

**Fix B (exact parity, larger):** port JTS `BufferInputLineSimplifier`, `OffsetSegmentGenerator`,
`OffsetSegmentString`, `OffsetCurveBuilder.getRingCurve` into `geo/` and union the raw curves with `polygon_overlay`
(`FillRule::Positive`).

**Status: Fix B implemented** (replaces Fix A and `geo::Buffer`; the old implementation is kept only as
`feature_merge::tests::legacy` for benchmarking). What it took, in order of impact:

1. `geo::buffer`: port of `BufferParameters`, `BufferInputLineSimplifier`, `OffsetSegmentGenerator` /
   `OffsetSegmentString` (all joins and caps), `OffsetCurveBuilder` and `BufferCurveSetBuilder` (erosion and
   inverted-ring checks, hole handling, `keepLargestArea`). The raw curves are bit-for-bit identical to JTS's
   (`geo::cg_algorithms` ports JTS's double-double orientation/intersection, `RobustLineIntersector`,
   `Distance`/`Angle` helpers; trig uses fdlibm via `libm`). The curves are resolved with `i_overlay`, positive fill.
2. Exact integer grid: `polygon_overlay::exact_grid` now scales so `max * scale <= 2^61` (was `2^52`), so every
   coordinate >= 1 px is exact on the grid. With the coarser grid, edges that are exactly collinear (or vertices
   exactly on another edge) in doubles became crossing or off-edge, adding vertices JTS doesn't have.
3. `geo::cascaded_polygon_union`: Java's `geometryCollection.union()` is `CascadedPolygonUnion` (STRtree order,
   pairwise classic `OverlayOp`). Each pairwise union's intersection points are computed from that pair's segments,
   so the order changes their last bits; a single-pass union matched Java's union exactly in only 69% of Boston
   groups, the cascade in 99.9%.
4. JTS-exact output vertices: after each overlay, vertices that are not input vertices are recomputed with JTS's
   `RobustLineIntersector` on the two input segments (i_overlay's are a few ulps off), and every ring is rotated to
   start where JTS's graph starts it (the first result edge along the input rings). Start points matter because
   the next buffer's input simplification and first join depend on them.
5. JTS's fixed-precision fallback: JTS's floating noding of the unbuffer curves is often invalid (rounded nodes
   leave crossings), building the result throws, and `BufferOp` retries with snap-rounding at 12 significant digits
   (output on a 1e-9 grid). `geo::buffer::noding` emulates JTS's `MCIndexNoder` + `IntersectionAdder` and
   `FastNodingValidator` to predict that (all 73 fallbacks on a Boston tile, 6 false positives in 2,211), and the
   port then snap-rounds the same way (hot pixels at curve vertices and intersections). This alone fixed ~70% of the
   remaining differences.

Results:

- `mergeNearbyPolygons` vs Java on the Boston/Jakarta test tiles (new `merge_*_matches_java` tests with Java's
  output stored in `tests/resources/buffer/`): polygon counts now exactly equal (1141, 947, 1073, 410); polygons
  without an identical Java polygon: 11/1141, 5/947, 19/1073, 11/410 (before: 195, 105, 176, 97 with Fix A).
  Almost all remaining ones differ in one vertex by one 1/16 px unit or one extra/missing vertex.
- Monaco OpenMapTiles: identical to Java (the 4 z13 `building` features now match).
- Switzerland: building 578 differing features (was 2,387), landuse 35 (was 404), landcover 17 (was 21), same
  feature count in every layer (`docs/switzerland-parity.md`).

Remaining causes: clusters of intersection points a few ulps apart that JTS's floating noding keeps and `i_overlay`
merges (they round differently later); wrong fallback predictions; JTS's floating noding being invalid without
JTS noticing (locationtech/jts#1183: JTS returns an empty buffer and Java falls back to a plain union, the port
returns the snap-rounded result); `Math.sin`/`cos` intrinsics vs fdlibm in limited mitre joins.

Performance (`feature_merge::tests::bench_merge_building_tiles`, release, min CPU of 7, loaded machine):
buffer/union/unbuffer+snap of every group of nearby buildings, port vs `geo::Buffer`: Boston 221 vs 190 ms (+16%),
176 vs 148 ms (+19%), 179 vs 148 ms (+21%); Jakarta 651 vs 1038 ms (-37%). Switzerland end to end: 158 s user CPU
(168 s CPU before).

**Regression test:** merge ~30°-rotated L-shapes/rectangles with `buffer = 0.5` and assert exact vertex counts
(rotated L: 6 + closing point; currently 8).

## 2. Java writes 28 empty tiles that Rust skips (OpenMapTiles `mountain_peak`)

- Java `MountainPeak.postProcess` (`MountainPeak.java:176-190`) sets out-of-buffer items to `null` in place; the list
  is non-empty, so `VectorTile.addLayerFeatures` (`VectorTile.java:502-510`) creates an empty layer, `isEmpty()` is
  false, and `TileArchiveWriter.java:305-307` writes the tile.
- Rust `MountainPeak::post_process` (`crates/planetiler-openmaptiles/src/layers/mountain_peak.rs:188-205`) uses
  `retain_mut`, emptying the vector; `build_vector_tile` (`collection/feature_group.rs:828-847`) only adds an empty
  layer when unscaling (not the post-processor) emptied it.

**Status: fixed** as described below (`mountain_peak.rs` `post_process`, plus
`test_empty_geometries_after_layer_post_processing_write_empty_tiles` in `core/tests/planetiler_tests.rs`). Not yet
re-checked on Switzerland.

**Fix (mountain_peak only):** replace `retain_mut` with an `iter_mut()` loop that sets out-of-buffer peaks'
geometry to an empty `VectorGeometry` (keep the group-count increment before the test, as Java does). The existing
`had_features && features.is_empty()` path then adds the empty layer. Update the test at `mountain_peak.rs:459`.
Don't change core generally — Java's FeatureMerge returning an empty list must not create layers.
