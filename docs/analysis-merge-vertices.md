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

**Fix A (recommended, small):** remove near-collinear vertices from `geo::Buffer` output in `buffer_geometry`
(`feature_merge.rs:595-602`), for both `+d` and `-d`. Drop vertex b between a and c when
`|cross(b-a, c-a)| / |c-a| <= eps` and `dot(b-a, c-b) > 0` (keep spikes); wrap the closing vertex; drop rings < 4
points. Use `eps ≈ H * 2^-25` with `H = 0.5 * max(bbox w, h) + 6|d|` (≈16-32 i_overlay grid units; far below the
≥1e-5 px offset of any real vertex on the 1/16 grid). JTS buffers never emit collinear vertices, so this matches.
Do not put it in `snap_and_fix` (Java keeps collinear points there).

**Fix B (exact parity, larger):** port JTS `BufferInputLineSimplifier`, `OffsetSegmentGenerator`,
`OffsetSegmentString`, `OffsetCurveBuilder.getRingCurve` into `geo/` and union the raw curves with `polygon_overlay`
(`FillRule::Positive`).

**Regression test:** merge ~30°-rotated L-shapes/rectangles with `buffer = 0.5` and assert exact vertex counts
(rotated L: 6 + closing point; currently 8).

## 2. Java writes 28 empty tiles that Rust skips (OpenMapTiles `mountain_peak`)

- Java `MountainPeak.postProcess` (`MountainPeak.java:176-190`) sets out-of-buffer items to `null` in place; the list
  is non-empty, so `VectorTile.addLayerFeatures` (`VectorTile.java:502-510`) creates an empty layer, `isEmpty()` is
  false, and `TileArchiveWriter.java:305-307` writes the tile.
- Rust `MountainPeak::post_process` (`crates/planetiler-openmaptiles/src/layers/mountain_peak.rs:188-205`) uses
  `retain_mut`, emptying the vector; `build_vector_tile` (`collection/feature_group.rs:828-847`) only adds an empty
  layer when unscaling (not the post-processor) emptied it.

**Fix (mountain_peak only):** replace `retain_mut` with an `iter_mut()` loop that sets out-of-buffer peaks'
geometry to an empty `VectorGeometry` (keep the group-count increment before the test, as Java does). The existing
`had_features && features.is_empty()` path then adds the empty layer. Update the test at `mountain_peak.rs:459`.
Don't change core generally — Java's FeatureMerge returning an empty list must not create layers.
