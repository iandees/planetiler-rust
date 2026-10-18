# Porting Planetiler to Rust

This repo is a port of [onthegomap/planetiler](https://github.com/onthegomap/planetiler) (Java) to Rust.
The upstream reference checkout lives at `../planetiler-ref/planetiler` (commit `d88cf89`, OpenMapTiles submodule
`373030b`).

## Layout

| Java module               | Rust crate                         |
|---------------------------|------------------------------------|
| `planetiler-core`         | `crates/planetiler-core`           |
| `planetiler-custommap`    | `crates/planetiler-custommap`      |
| `planetiler-openmaptiles` | `crates/planetiler-openmaptiles`   |
| `planetiler-examples`     | `crates/planetiler-examples`       |
| `planetiler-benchmarks`   | `crates/planetiler-benchmarks`     |
| `planetiler-dist`         | `crates/planetiler` (CLI binary)   |

The third-party Java MLT library Planetiler depends on (`org.maplibre:mlt:0.0.10`) is ported into
`planetiler-core/src/mlt/` (see "MLT output notes").

Inside `planetiler-core`, every Java file `com/onthegomap/planetiler/<pkg>/<Name>.java` maps to
`src/<pkg>/<snake_name>.rs` (e.g. `reader/osm/PbfDecoder.java` -> `src/reader/osm/pbf_decoder.rs`). `Struct.java` is
`reader/structs.rs`. Files that are pure Java plumbing (functional interfaces, `Try`, `Memoized`, stream wrappers, ...)
were not given Rust files; use the Rust idiom instead.

## Conventions

- **Names**: snake_case of the Java name. Java getters drop `get` only when it reads naturally; otherwise keep it
  (`get_world_x`). Overloads get descriptive suffixes (`has_tag_str`, `has_tag_any`).
- **Geometry**: JTS `Geometry` -> `geo_types::Geometry<f64>`; JTS `Envelope` -> `crate::geo::envelope::Envelope`
  (supports "null"/empty). Algorithms come from `geo` (`BooleanOps`, `Buffer`, `Validation`, `Area`, ...),
  `i_overlay` and `rstar`. When `geo` lacks something JTS provides, port the JTS algorithm into `geo/`.
- **Tags / attributes**: Java `Map<String, Object>` -> `crate::value::Tags` (`IndexMap<String, Value>`); `Object` ->
  `crate::value::Value`. Use the `tags!{}` macro in tests.
- **Errors**: `GeometryException` -> `crate::geo::geometry_exception::GeometryException` /
  `GeometryResult<T>`. Everything else uses `anyhow::Result` (or a `thiserror` enum where callers need to match).
- **Stats**: `Arc<dyn Stats>` (`StatsRef`); use `InMemoryStats::arc()` in tests.
- **Config**: `PlanetilerConfig` fields are snake_case of the Java record components.
- **Threads**: Java `Worker`/`WorkerPipeline`/`WorkQueue` are ported in `worker/`; prefer `std::thread::scope`,
  `crossbeam-channel` and `rayon` for internals.
- **Logging**: `tracing` (`info!`, `warn!`, ...), mirroring the Java log messages.
- **Tests**: port the Java unit tests in `planetiler-core/src/test/java/...` for each file into a `#[cfg(test)]` module
  in the same Rust file (or `tests/` for integration tests). Test resources are in `planetiler-core/src/test/resources`.
- **Formatting**: `cargo fmt` (see `rustfmt.toml`); `cargo clippy` should be clean.
- Mark intentionally-deferred work with `// TODO(port): ...`.
- **Vendored crates**: `vendor/i_float` is `i_float` 5.0.0 with a fast exact two-word division
  (`UIntNumber::div_rem_wide`; upstream divides bit by bit, which was ~12% of building merge time in the i64
  overlay engine's segment intersections), used via `[patch.crates-io]`. Results are identical; drop the patch if
  upstream speeds this up.

## Status

See the table at the bottom of this file; update it as modules are completed.

| Area | Status |
|------|--------|
| Foundation types (`value`, `geo::{envelope, geometry_type, geometry_exception, tile_coord, tile_extents}`, `util::hilbert`, `stats` trait, `config` struct) | done |
| `vector_tile` (VectorTile, VectorGeometry, VectorGeometryMerger, Feature, encode/decode, `toMltInput` / MLT encoding), `util::{tile_size_stats (incl. `computeMltTileStats`), layer_attr_stats, tileset_summary_statistics}` | done |
| `mlt`: port of the Java MLT library `org.maplibre:mlt:0.0.10` (MltConverter, Integer/String/Property/Geometry encoders, stream metadata, JavaFastPFOR 0.3.10 FastPFOR+VariableByte, the Java FSST encoder, earcut4j 3.0.0) + a decoder for its output; `TileFormat::Mlt` end-to-end with all `mlt_*` options | done (byte-identical to Java; see "MLT output notes") |
| `reader::{structs, with_tags, with_geometry_type, json_conversion}` (dotted keys via `WithTags::get_tag_nested`; `get_tag` stays a direct lookup) | done (helpers use `util::parse`/`util::imposm3_parsers`; Struct has no Instant/LocalDate/UUID variants) |
| `reader::osm::{pbf_decoder, pbf_field_decoder, osm_element, osm_header, osm_input_file, osm_block_source, poly_file_reader}` (raw/zlib/lz4/zstd, eager or mmap lazy reads; hand-written zero-copy protobuf decoder, zlib via libdeflate) | done |
| `expression::{expression, multi_expression, data_type, simplifiable}` (input trait `ExpressionInput`) | done |
| `planetiler-benchmarks` `benchmark_osm_read` binary (runs `OsmReader::pass1` with a null profile like Java) | done |
| `collection` (append stores, `LongLongMap` + impls, `LongLongMultimap`, heaps, `LongMerger`, `IntRangeSet`, `SortableFeature`, `FeatureSort`, `ExternalMergeSort`) + `LongLongMapBench` (`planetiler-benchmarks` bin `long_long_map_bench`) | done |
| `config` (`Arguments`, `PlanetilerConfig::from`, `Bounds`) | done |
| `stats` (`Stats`, `InMemoryStats`, `PrometheusStats`, `DefaultStats`, `ProgressLoggers`, `ProcessInfo`, `Timers`, `Timer`, `ProcessTime`, `Counter`) | done (no GC/heap stats; per-thread CPU via thread registry) |
| `worker` (`Worker`, `WorkerPipeline`, `WorkQueue`, `WeightedHandoffQueue`, `Distributor`) | done (`processAndWrite` is `collection::feature_group::process_and_write`) |
| `util` (format, ansi_colors, parse, imposm3_parsers, var_int, hashing, sort_key, merging_range_map, sliding_window, zoom_function, cache_by_zoom, bin_pack, common_string_encoder, byte_buffer_util, gzip, yaml, build_info, madvise, memory_estimator, file_utils, file_watcher, resource_usage, tile_weights, downloader, aws_osm, geofabrik, overture_stac, top_osm_tiles) | done |
| benchmarks: `benchmark_var_int`, `benchmark_tile_coord` | done |
| Archives (`archive::{tile, tile_encoding_result, readable_tile_archive, writeable_tile_archive, tile_archive_config, tile_archive_metadata(_de_ser), tile_archives}`, `mbtiles`, `pmtiles`, `files`, `stream`, `util::compare_archives`, `geo::tile_order`) | done (`TileArchiveMetadata::from_profile`; archive options are `config::arguments::Arguments`; `util::layer_attr_stats` re-exports the archive `FieldType`, and the two `VectorLayer` types stay separate on purpose (insertion-ordered vs sorted fields); `mbtiles::{verify, compare}` decode geometries like Java). Difference: `CompareArchives` (`planetiler compare`) skips byte-identical tiles and compares the others as normalized feature multisets per layer (layers paired by name; key `(geometry type, normalized geometry, tags[, id])`, ring start point / ring order / feature order / tag order ignored, `--ignore-ids`), reporting per-layer only-in-1 / only-in-2 feature counts by zoom with examples (see its module docs); it is the Java parity check, see `docs/switzerland-parity.md` |
| Benchmarks `benchmark_mbtiles_writer`, `benchmark_mbtiles_read`, `benchmark_pmtiles` (`crates/planetiler-benchmarks/src/bin`) | done |
| `geo` (geo_utils, simplifiers, smoothers, line_splitter, geometry_pipeline/transformer, point/polygon index, unit, with_geometry, tile_order, simplify_method) + JTS ports (`is_valid`, `polygon_overlay` fix/precision-reduce/boolean ops, `interior_point`/centroid, `maximum_inscribed_circle`, `normalize`, `precision_model`, `wkt`, `crs`) + `util::loop_line_merger` + `geo::test_utils` | done (see notes below) |
| `reader::osm::{osm_multipolygon, osm_way_splitter, osm_node_bounds_provider}` | done (`NodeLocationProvider` trait lives in `osm_multipolygon`, re-exported from `osm_reader`) |
| `reader::osm::{osm_reader, osm_phaser}` (+ `OsmReaderTest`, `OsmPhaserTest`, monaco end-to-end test matching Java feature counts / data errors), `benchmark_osm_reader` bin + `bench/java/BenchmarkOsmReader.java` | done (see notes below) |
| `util::{translations, language_utils, thread_local_transliterator, wikidata}` | done (`wikidata::fetch` takes a `Fn(&OsmElement) -> bool` filter; `fetch_for_profile` / `profile_filter` adapt a `Profile`) |
| Feature pipeline spine: `profile` (`Profile`, `NullProfile`), `feature_processor`, `forwarding_profile`, `feature_collector`, `reader::{source_feature, simple_feature}`, `reader::osm::osm_relation_info` (+ `RelationMember`), `render::{feature_renderer, tiled_geometry, geometry_coordinate_sequences, rendered_feature}`, `collection::feature_group` | done (see notes below) |
| `reader::{simple_reader, source_feature_processor, shapefile_reader, geo_package_reader, natural_earth_reader}`, `reader::geojson::*`, `reader::parquet::*` + benchmark `benchmark_simple_readers` | done as *decoders* emitting `ReaderFeature`; wiring into `SimpleFeature`/`Profile`/`FeatureGroup` is `TODO(port)` (see notes) |
| `archive::tile_archive_writer` (`TileArchiveWriter.writeOutput`), `validator::{schema_specification, base_schema_validator, java_profile_validator}`, `testing` harness + `tests/planetiler_tests*.rs` (the `runWithReaderFeatures` subset of `PlanetilerTests`), benchmark `benchmark_tile_archive_writer` | done |
| `planetiler` (`Planetiler.java` orchestrator) + `tests/planetiler_runner_tests.rs` (testPlanetilerRunner*, MemoryCheck*, ReuseFeatureDb) + `tests/planetiler_osm_tests.rs` (all OSM `PlanetilerTests`) + `testing::run_with_osm_elements`, `TileSizeStats.main` (`tile_size_stats::main`), `CompareArchives.main` (`compare_archives::main`) | done (see "Planetiler orchestrator notes"; deferred: csv/tsv/json/proto/pbf variants of testPlanetilerRunner, testExceptionWhileProcessingOsm is `#[ignore]`d) |
| `planetiler` CLI crate (`planetiler-dist` `Main.java` task dispatch) | done (`verify`/`verify-schema` run the custommap `SchemaValidator`; stubs that error: `benchmark-mapping`, `benchmark-longlongmap` (use the benchmarks crate), `top-osm-tiles` (needs xz)) |
| `planetiler-examples` (all 9 examples incl. `ToiletsOverlayLowLevelApi`, `OvertureBasemap`) + all example tests | done |
| `planetiler-openmaptiles`: `Generate` (generator + vendored schema), generated `OpenMapTilesSchema`/`Tables`, `OpenMapTilesProfile`, `OpenMapTilesMain` (`setup`/`configure`/`run`/`main`), all 16 layers, `OmtLanguageUtils`, `Utils`, `VerifyMonaco`, all unit tests (249), `OpenMapTilesTest` (`tests/open_map_tiles_test.rs`) | done (see notes below) |
| `planetiler-custommap` (configschema, contexts, TagValueProducer, TypeConversion, ConfigExpression/Parser, BooleanExpressionParser/Script, CEL stdlib on the `cel` crate, ConfiguredFeature, ConfiguredProfile, ConfiguredMapMain (`configure`, `create_planetiler`, `main`), bundled samples + JSON schemas, `ConfiguredMapTest`, `SchemaValidator` CLI + `--watch`) | done (see notes below) |
| `feature_merge` (`FeatureMerge`: line merging via `LoopLineMerger`, nearby/overlapping polygon merging with buffer/union/unbuffer, multi-point/line/polygon merging, `remove_points_outside_buffer`, `group_by_attrs`, `extract_connected_components`) + full `FeatureMergeTest` | done (see notes below) |
| `reader::{simple_reader, source_feature_processor, shapefile_reader, geo_package_reader, natural_earth_reader}`, `reader::geojson::*`, `reader::parquet::*` + benchmark `benchmark_simple_readers` | done, including profile/`FeatureGroup` wiring (`source_feature_processor::process_files`, per-reader `process` entry points, `impl SourceFeature for ParquetFeature`); see notes for the orchestrator API |

### `geo` notes

- JTS `isValid` -> `geo::is_valid` (JTS semantics: no ring self-touch, holes may touch at single points as long as the
  interior stays connected). Prefer it over `geo::Validation`, which differs and is O(n^2).
- `buffer(0)` -> `polygon_overlay::buffer_zero` (positive winding; input vertices that touch an output edge are
  put back as collinear vertices, since JTS's noding makes them graph nodes), `GeometryFixer.fix` ->
  `polygon_overlay::geometry_fixer` (non-zero winding), `GeometryPrecisionReducer.reduce` ->
  `polygon_overlay::reduce_precision`. Overlay output: shells CW, holes CCW, OGC-valid.
- `reduce_precision` follows JTS's `PrecisionReducerTransformer`: each polygon / multipolygon (also inside a
  collection) is reduced like `OverlayNG(geom, pm)` with area results only, and lines and points are rounded pointwise
  (JTS's `GeometryPrecisionReducer` does not node lines either), with JTS's result types (`LINESTRING EMPTY` for a
  collapsed line, `POLYGON EMPTY` for a collapsed multipolygon, single-part multis become the part). The polygon
  noding is `geo::snap_rounding::snap_round`, a port of JTS `SnapRoundingNoder`: `MCIndexNoder` monotone chains with
  `SnapRoundingIntersectionAdder` (interior intersections and vertices within `gridSize / 100` of another chain's
  segment become nodes and hot pixels), hot pixels at every vertex, then every noded segment snapped to every hot pixel
  it passes through (`HotPixel.intersects`), ordered along the segment like `SegmentNodeList`
  (`SegmentPointComparator`). JTS's hot pixel node flags only decide where noded strings are split, so they are not
  modelled. Instead of OverlayNG's graph and labelling, the snapped rings are resolved by `i_overlay` (positive
  winding, each ring oriented by JTS's depth delta: `Orientation.isCCW` of the input ring and whether it is a hole),
  which gives the same polygons for valid input. Unlike JTS it never fails, so the fallback paths in
  `snap_and_fix_polygon` are rarely hit. `polygon_overlay::tests::reduce_precision_matches_java_on_*` compare
  `reduce_precision` and `snap_and_fix_polygon` with Java (`bench/java/SnapRoundCases.java`, cases in
  `tests/resources/snapround`): all 4,000 synthetic cases (near-touching and self-touching polygons, slivers, spikes,
  holes near the shell, invalid bow-ties, lines, collections) and all 1,456 tile polygons match exactly (normalized),
  except `reduce_precision` of 14 invalid tile polygons, where what JTS keeps depends on how OverlayNG labels the
  inconsistent sides of self-overlapping rings (Planetiler only reduces valid geometries: `snapAndFixPolygon` fixes
  invalid input first). On all 109,910 polygons that took the snap-rounding path in an OpenMapTiles Switzerland run,
  `snap_and_fix_polygon` matches Java exactly (20 differed before), and `reduce_precision` differs on 41 invalid
  inputs (3,084 before).
- Empty geometries: `GeometryCollection(vec![])` (Java `EMPTY_GEOMETRY`), `Polygon` with empty exterior, empty
  `LineString`; geo has no empty point so `EMPTY_POINT` is an empty `MultiPoint`.
- CRS support (`geo::crs`) is limited to EPSG:4326/4269/3857 (+aliases), PROJ strings via `proj4rs`, and simple
  `GEOGCS`/web-mercator `PROJCS` WKT.
- JTS `BufferOp` -> `geo::buffer::buffer(geom, distance, &BufferParameters)`: port of the offset curve generation
  (`BufferParameters`, `BufferInputLineSimplifier`, `OffsetSegmentGenerator`/`OffsetSegmentString`,
  `OffsetCurveBuilder`, `BufferCurveSetBuilder`; raw curves bit-for-bit identical to JTS) with `i_overlay` (positive
  winding, `polygon_overlay::positive_winding_region`) instead of JTS's noding/graph. Intersection vertices are
  recomputed with JTS's `RobustLineIntersector` and rings rotated to JTS's start point; JTS's floating-noding failure
  (and fallback to snap-rounding at 12 digits) is predicted by emulating its noder and `FastNodingValidator`
  (`geo::buffer::noding`), and the fallback is `ScaledNoder(SnapRoundingNoder)` like JTS
  (`polygon_overlay::positive_winding_region_scaled_snap_rounded`, on `geo::snap_rounding`). See the module docs for the remaining differences. A single raw curve that is simple
  (`noding::is_simple_closed_curve`) with every vertex exact on the overlay grid skips the overlay
  (`polygon_overlay::simple_curve_positive_winding_region`: the region is the curve itself); `buffer` tests compare
  it with the general path.
- `InputEdges` (overlay output vertex refinement / ring start matching) finds input segments near a vertex with
  `geo::segment_grid::SegmentGrid` (a lazily built uniform grid; the first queries scan linearly) instead of an
  `rstar` tree. Candidates come back in input order, and when the refined vertex could depend on the candidate
  order (a tie between different intersection points, or a pair whose intersection depends on segment order) it
  falls back to the R-tree order the port used before, so results are unchanged.
- JTS `geometryCollection.union()` for polygons -> `geo::cascaded_polygon_union::union` (port of
  `CascadedPolygonUnion` with the `STRtree` item order; pairwise `polygon_overlay::union_pair`). Use it where Java
  calls `union()` and exact vertices matter; `polygon_overlay::union_all` is a faster single-pass union.
- JTS primitives with identical floating point behaviour (`DD`, `CGAlgorithmsDD.orientationIndex`/`intersection`,
  `RobustLineIntersector`, `Distance`, `Angle`, `Area.ofRingSigned`) are in `geo::cg_algorithms`; trig uses `libm`
  (fdlibm, like `StrictMath`) since the platform `sin`/`cos`/`atan2` can differ in the last bit.
- `polygon_overlay`'s exact grid scales coordinates so `max * scale <= 2^61` (i64 engine), so coordinates >= 1 are
  exact; a coarser grid turns exactly-collinear edges into spurious crossings.
- JTS `Polygonizer` -> `geo::polygonizer::Polygonizer` (`new()` / `with_extract_only_polygonal(bool)`,
  `set_check_rings_valid`, `add_geometry` / `add_all` / `add_line`, then `polygons()`, `geometry()`, `dangles()`,
  `cut_edges()`, `invalid_ring_lines()`; ports `PolygonizeGraph`, `EdgeRing`, `HoleAssigner` and the `planargraph`
  pieces they use, with all of JTS `PolygonizerTest`). Input must be noded, as in JTS. Directed edges are visited in
  insertion order (JTS uses a `HashSet`, so ring discovery order and start points vary between JVM runs); badly-noded
  input stops a ring walk instead of tripping a JTS assertion. Used by OpenMapTiles `boundary` to build country
  polygons for `adm0_l`/`adm0_r` exactly like Java (polygonize the relation's ways, union the faces).
- `LoopLineMerger` stores both directions of an edge as one pair (`id ^ 1` is the reverse) sharing a coordinate range
  in one arena, node edge lists are inline for up to 4 edges, and node lookup uses one `hashbrown` map; output is
  identical to the straight port. `VWSimplifier` reuses thread-local scratch buffers.

### `feature_merge` notes

- Functions take `Vec<Feature>` by value and move unchanged features to the output. Overloads:
  `merge_line_strings(features, min_length, tolerance, buffer)`, `merge_line_strings_resimplify(.., resimplify)`,
  `merge_line_strings_dynamic(features, |tags| min_length, tolerance, buffer)`,
  `merge_line_strings_with(.., resimplify, Option<GeometryPipelineRef>)`; `merge_overlapping_polygons`,
  `merge_nearby_polygons(features, min_area, min_hole_area, min_dist, buffer)`,
  `merge_nearby_polygons_with(.., &dyn Stats, Option<&dyn GeometryPipeline>)` (return `GeometryResult`).
- JTS `BufferOp` with mitre joins -> `geo::buffer` (JTS offset-curve port, see geo notes) and `union()` ->
  `geo::cascaded_polygon_union` (docs/analysis-merge-vertices.md "Fix B"). `merge_*_matches_java` compare
  `merge_nearby_polygons` with Java's output on the Boston/Jakarta/protomaps538 tiles (stored in
  `tests/resources/buffer/`; ~1% of polygons differ, mostly by one vertex), and `bench_merge_building_tiles`
  benchmarks against the previous `geo::Buffer` implementation (`tests::legacy`). `benchmark_feature_merge`
  (planetiler-benchmarks) reports single-thread CPU time per merged building tile for A/B comparisons.
- The proximity grouping decides `distance <= min_dist` for small polygon pairs with a brute-force segment scan
  (robust crossing test + vertex-to-segment distances, `polygons_within_distance`) and only calls `geo`'s
  R-tree-based `Euclidean` distance when the boundary distance is within 1e-9 (relative) of `min_dist`, so the
  decisions are the same.
- `buffer_union_unbuffer` succeeds on 3 of the protomaps538 inputs where JTS's floating unbuffer silently returns an
  empty result (locationtech/jts#1183) and Java falls back to a plain union; the Rust result fills sub-pixel gaps
  (area up to ~3% larger than the input), so that test allows a 3% area increase instead of 1%. The `@Slow`
  Boston/Jakarta tests are `#[ignore]`d (run with `--release -- --ignored`, ~1s) and match Java's polygon counts
  exactly.


### OSM multipolygon / way splitter notes

- `OsmMultipolygon.build(rings, nodeCache, osmId[, minGap])` -> `osm_multipolygon::build(&[Vec<i64>], &dyn NodeLocationProvider,
  osm_id)` / `build_with_gap(..)`; `build(List<CoordinateSequence>)` -> `build_from_coordinates(&[Vec<Coord>])`. Same
  `GeometryException` stats as Java: `osm_invalid_multipolygon_empty`, `osm_invalid_multipolygon_empty_after_fix`,
  `osm_invalid_multipolygon_not_closed` (verbose) and `osm_invalid_multipolygon` (missing nodes / other errors).
- `OsmReader.NodeLocationProvider` -> `osm_multipolygon::NodeLocationProvider` (`get_coordinate` returns
  `anyhow::Result`; implemented for `Fn(i64) -> Option<Coord>` closures and `LongLongMapLocationProvider(&dyn
  LongLongMap)` which decodes `encode_flat_location` values to world coordinates).
- JTS `PreparedPolygon.contains` -> envelope + point-in-ring rejection then `geo::PreparedGeometry::relate(..).is_contains()`.
  JTS `TopologyException` (which triggers the "fix every ring and retry" path) maps to a panic inside `geo`'s relate,
  caught with `catch_unwind`.
- `OsmWaySplitter` is a trait (`writer_for_thread()` returns a `PerThreadWriter` that flushes on drop,
  `get_split_indices(&[i64]) -> Vec<usize>`); `map_splitter()` and `roaring_bitmap_splitter()` (`roaring` crate,
  per-partition `RwLock` instead of Java's lock objects).

### Translations / transliteration notes

- `Translations` keeps the Java API (`null_provider`, `default_provider`, `add_fallback_translation_provider`,
  `get_translations`/`add_translations` on `Tags`, `care_about_language`, `set_should_transliterate`,
  `set_extra_name_tags`) plus builder-style `with_*` variants. `TranslationProvider::get_name_translations(&Tags)`
  returns `Option<Cow<BTreeMap<String, String>>>`; closures `Fn(&Tags) -> BTreeMap<..>` are providers.
  `Translations.transliterate` -> `translations::transliterate(&str)`.
- Transliteration: Java uses ICU4J `Any-Latin`. Rust uses ICU4X (`icu_experimental::transliterate`, CLDR rules,
  compiled data) and emulates ICU's `AnyTransliterator`: split into script runs (Common/Inherited join the
  surrounding run), Latin runs are untouched, other runs use `und-Latn-t-und-<script>` (Han uses
  `und-Latn-t-und-hans`, i.e. CLDR Han-Latin). Outputs match ICU4J for the scripts checked (Han `日本` -> `rì běn`,
  Cyrillic, Greek with diacritics, Hangul, Hiragana/Katakana, Arabic, Hebrew, Devanagari, Georgian). Known
  differences: scripts ICU4X can't load (e.g. Thai, whose CLDR rules need a dictionary segmenter) or that have no CLDR
  transform fall back to `any_ascii` (ICU uses its own Thai rules and leaves scripts without a transform unchanged);
  Common characters at run boundaries are given to one run only (ICU feeds them to both neighbouring runs), which can
  change spacing slightly around punctuation. `ThreadLocalTransliterator::get_instance` supports `Any-Latin` and raw
  ICU4X BCP-47 transform IDs; instances are `!Send` and cached per thread.
- `LanguageUtils.isValidLanguageTag` (Java `Locale.Builder.setLanguageTag`) is a hand-written BCP 47 well-formedness
  parser (legacy/grandfathered tags map to their preferred language); `ONLY_LATIN`/`removeLatinCharacters` use
  `regex` with `\p{sc=Latin}` and ASCII-only `\s` like Java.

### Wikidata notes

- `Wikidata.fetch(infile, outfile, config, profile, stats[, maxAge, updateLimit])` -> `wikidata::fetch(&OsmInputFile,
  &Path, &PlanetilerConfig, filter: Fn(&OsmElement) -> bool, StatsRef)` / `fetch_with_max_age(..)` /
  `fetch_with_client(.., Box<dyn WikidataClient>)`. `Wikidata.load(path)` -> `wikidata::load(&Path)`,
  `load(reader, maxAge, updateLimit, clock)` -> `load_from_reader(BufRead, Duration, usize, now_millis)`.
- `Wikidata.Client` -> `WikidataClient` trait (`send(&WikidataRequest) -> anyhow::Result<Vec<u8>>`), default
  `UreqWikidataClient`. Any send/parse error is retried `http_retries` times (Java retries `IOException`s, which
  includes JSON errors). Output lines are written sorted by QID.
- `WikidataTranslations` implements `TranslationProvider`; update times are epoch millis (`i64`).
### Feature pipeline notes (profile / collector / renderer / feature group)

**Source features.** `SourceFeature` is a trait (`reader::source_feature`): `WithGeometry` (itself a subtrait of
`WithGeometryType`) + `ExpressionInput` + `Debug`, with required `base() -> &SourceFeatureBase` and
`as_any_feature()`. `SourceFeatureBase` holds tags, `source`/`source_layer` (`Option<Arc<str>>`, share one `Arc` per
reader), `relation_infos: Vec<RelationMember<dyn OsmRelationInfo>>`, `id` and the `GeometryCache`. A reader feature
type owns a base, implements `WithGeometryType`, `WithGeometry` (`world_geometry`, `lat_lon_geometry` - compute lazily
into `OnceLock`s - `geometry_cache`, optionally `compute_line`/`compute_polygon` returning `Ok(None)` to mean "use the
world geometry as-is") and `SourceFeature` (override `vector_tile_feature_id`, `original_element`, `is_full_way`,
`is_split_way` for OSM), then calls `impl_source_feature_traits!(Type)` to get `WithTags` + `ExpressionInput`.
The source name is `ExpressionInput::source()`. Generic accessors live on `SourceFeatureExt` (blanket impl, works on
`dyn SourceFeature`): `relation_info::<T>()` / `relation_info_with_super::<T>(bool)` downcast relation infos.
`SimpleFeature` covers known-geometry readers: `create(lat_lon, tags, source, layer, id)` (names accept `&str`,
`String`, `Arc<str>` or `Option`s via `IntoName`), `create_world`, `from_lat_lon_geometry[_with_id]`,
`from_world_geometry[_with_id]`, `create_tags[_with_id]`, `create_fake_osm_feature`, `from_base_lat_lon/world`.

**OSM relations.** `OsmRelationInfo: Any + Send + Sync + Debug` (`id`, `estimate_memory_usage_bytes`,
`vector_tile_feature_id`); `Profile::preprocess_osm_relation` returns `Option<Vec<Arc<dyn OsmRelationInfo>>>`;
the reader wraps them as `RelationMember::new(role, arc)` / `with_path(role, arc, parent_ids)`
(`RelationMember<T>::erase()` converts a typed member).

**Processing a feature (reader side).**
```rust
let factory = feature_collector::Factory::new(config.clone(), stats.clone()); // shared, Arc<PlanetilerConfig>
let renderer = FeatureRenderer::new(config.clone(), stats.clone());           // shared by all threads
// per worker thread:
let mut writer = feature_group.feature_writer()?;          // encoder + FeatureSortWriter
let source_feature = MyFeature { .. };
let mut collector = factory.get(&source_feature);          // FeatureCollector<'_> borrows the source
profile.process_feature(&source_feature, &mut collector);
for feature in collector.iter() {                          // skips omitted / empty features
    renderer.accept(feature, &mut |rendered| writer.write(&rendered).expect("write"));
}
// after a source: profile.finish(name, &factory, &mut |feature| renderer.accept(feature, &mut ...));
writer.close()?;
```
For the Java `processAndWrite` threading pattern use `feature_group::process_and_write(builder, inline_writes,
process_threads, write_threads, Arc<FeatureGroup>, handler)` where the handler gets `(&mut WorkQueueReader<O>,
&mut dyn FeatureSink)` and encodes with its own `group.new_rendered_feature_encoder()` +
`sink.write(&mut encoder, &rendered)`. After the pipeline finishes, reclaim the group (`Arc::into_inner`) and
call `prepare()`.

**FeatureCollector / Feature.** Builders (`point`, `line`, `split_line[_with_renumber]`, `partial_line`, `polygon`,
`any_geometry`, `centroid`, `centroid_if_convex`, `point_on_surface`, `innermost_point[_with_tolerance]`,
`line_midpoint`, `point_along_line`, `geometry[_with_renumber]`) return `&mut Feature<'a>` for chaining; geometry
errors are logged to stats and produce an empty, never-emitted feature. Geometries from the source are borrowed
(`Cow`). Attribute values are `AttrValue` (`Value`, `Struct` -> raw value, or `ZoomFunction<T>`); `Null` is ignored.
Attributes are kept sorted by key; `get_attrs_at_zoom(z) -> &Arc<Tags>`. Zoom functions use `u32` zooms;
numeric per-zoom settings take `Option<ZoomFunction<f64>>` (`ZoomFunction<i32>` for label grid limits). Linear
ranges: `feature.linear_range(a, b).set_attr(..).set_min_zoom(..).entire_line()`. `omit()` sets a flag.
`feature_collector::test_support::{to_map, assert_submap}` port `TestUtils.toMap`/`assertSubmap`.

**Profile.** Methods take `&self` (profiles are shared across threads: `Send + Sync`). Post-processing works in
place: `post_process_layer_features(layer, zoom, &mut Vec<vector_tile::Feature>) -> GeometryResult<()>` and
`post_process_tile_features(tile, &mut LayerFeatures /* BTreeMap<String, Vec<Feature>> */)`; Java's "set element
to null" = remove it, "return null" = leave unchanged. `finish(source, &Factory, &mut dyn FnMut(&Feature))`.
`Profile` is implemented for `Arc<P>`. `ForwardingProfile` is a concrete struct that profiles embed and forward to;
handlers are `Arc<dyn Handler>` exposing capabilities via `as_feature_processor()` etc. (see module docs; closure
helpers `*_fn`).

**Rendering.** `RenderedFeature { tile, vector_tile_feature: Arc<vector_tile::Feature>, sort_key, group:
Option<Group> }`; filled tiles of one polygon/zoom share one `Arc` so the encoder only encodes it once.
`TiledGeometry` (`slice_into_tiles`, `slice_points_into_tiles`, `get_covered_tiles`, `CoveredTiles`) and
`geometry_coordinate_sequences` (`Groups = Vec<Vec<Vec<Coord>>>`) are public for other users (e.g. `Bounds` shapes).
A line or polygon whose coordinates (plus the neighbor buffer) all fall in one tile skips the striped clipping
(`slice_single_tile_group`, same output; a test compares both on random inputs).
JTS `LinearRing` semantics: when simplification collapses a polygon's shell, JTS's `GeometryTransformer` returns the
surviving rings as `LinearRing`s, and Java's renderer then treats them as non-polygonal (line slicing, unsquared min
size) but `extractGroups` turns each `LinearRing` into a polygon filtered by area. geo has no `LinearRing`, so
`FeatureRenderer` passes `linear_rings = input was polygonal && output is not` down to
`gcs::extract_groups_with_rings`, which treats closed linestrings with >= 4 points as such rings. (This was the one
extra feature Rust emitted for Switzerland with the `emit` benchmark profile: relation 9683714, a sliver whose shell
collapses at z14 leaving only its hole.)

**FeatureGroup (TileArchiveWriter side).** `FeatureGroup::new_in_memory(tile_order, profile: Arc<dyn Profile>,
config: Arc<PlanetilerConfig>, stats)` / `new_disk_backed(tile_order, dir, profile, config, stats, reuse)`. Sort key
layout and msgpack value encoding are byte-compatible with Java. Reading:
```rust
group.prepare()?;                                  // &mut, sorts once
let mut tiles = group.parallel_iterator(threads)?; // or group.iter()? / iter_prepared()
let mut last: Option<TileFeatures> = None;
for tile in &mut tiles {
    if tile.has_same_contents(last.as_ref()) { /* reuse previous encoded bytes */ }
    let vector_tile = tile.get_vector_tile_with_stats(layer_stats.as_ref()); // runs profile post-processing
    let bytes = vector_tile.encode();
    last = Some(tile);
}
tiles.join()?;
```
`TileFeatures` is `Send + 'static` (holds an `Arc` of shared state), so it can be handed to encoder threads.

### Simple readers (shapefile, geopackage, natural earth, geojson, parquet) notes

- Readers are *decoders*: they implement `reader::simple_reader::SimpleReader` (`source_name()`, `feature_count() ->
  Result<u64>`, `read_features(&mut dyn FnMut(ReaderFeature) -> Result<()>)`, `read_all()`) and emit
  `ReaderFeature { geometry (lat/lon), tags, source_layer: Option<Arc<str>> (one shared Arc per layer), id }`.
  Readers may decode in parallel internally but always call the sink from the calling thread.
  `ReaderFeature::into_simple_feature(&Arc<str> source)` moves geometry/tags/layer into a `SimpleFeature` (Java
  `SimpleFeature.create(geometry, tags, sourceName, sourceLayer, id)`).
  `read_features_within(bounds, skipped, next)` (default: `read_features`) lets a reader drop features whose lat/lon
  envelope cannot intersect `config.bounds` before decoding them, reporting the count to `skipped` so progress
  still reaches 100%; the processor filters again, so it is purely an optimization (`NaturalEarthReader` uses
  `geo_utils::wkb_envelope`, a no-allocation envelope of the WKB that contains the parsed geometry's envelope).
- Unlike Java (which opens one reader per path to count features and another one to read them),
  `SourceFeatureProcessor` keeps the readers it opened to count and reads from those, so expensive setup happens
  once per run: Natural Earth is unzipped once (Java unzips it twice), and a bounds-filtered shapefile keeps the raw
  records that passed the filter during `feature_count` (up to 512 MB) so the `.shp` is inflated once.
- **Profile wiring** (`reader::source_feature_processor`): `process_files(name, paths, reader_factory, &writer,
  &config, &profile, stats)` is Java's static `SourceFeatureProcessor.processFiles`: `read` threads ->
  `process_queue` -> `feature_group::process_and_write` (inline when `config.parallel_temp_io`, else via write
  threads). Each process thread owns a `ProfileProcessor` (`feature_collector::Factory`, `RenderedFeatureEncoder`,
  shared `Arc<FeatureRenderer>`), skips features whose lat/lon envelope misses `config.bounds`, runs
  `profile.process_feature` (panics are caught and logged like Java's `catch (Exception e)`; write errors fail the
  pipeline) and renders into the sink. Loggers: `read` (rate/percent), `write` (`num_features_written`), file size
  (`impl DiskBacked for FeatureGroup`), process + pipeline stats. Afterwards `profile.finish(name, ...)` is rendered
  into `writer.feature_writer()` on the calling thread. Returns immediately if `!profile.cares_about_source(name)`
  (Java checks this in `Planetiler`, doing it here too is harmless). The closure-based variant is
  `process_files_with(name, paths, reader_factory, config, stats, processor_factory)` /
  `ParquetReader::process_with(paths, config, cares_about, processor_factory)`.
- **API for the `Planetiler` orchestrator** (all take `writer: &Arc<FeatureGroup>`, `config: &Arc<PlanetilerConfig>`,
  `profile: &Arc<dyn Profile>`, `stats: StatsRef` and return `anyhow::Result<()>`; after the last source reclaim the
  group with `Arc::into_inner` and call `prepare()`):
  - `addShapefileSource(projection, name, path/glob)`: `ShapefileReader::process_with_projection(projection:
    Option<&str>, name, paths: Vec<PathBuf>, writer, config, profile, stats)` (or `ShapefileReader::process(name,
    paths, ..)` for the `.prj` projection). Expand globs / zip contents with `file_utils::walk_path_with_pattern`.
  - `addGeoPackageSource(projection, name, path/glob)`: `GeoPackageReader::process(projection, name, paths, tmp_dir,
    writer, config, profile, stats, keep_unzipped)`.
  - `addNaturalEarthSource(name, path)`: `NaturalEarthReader::process(name, &path, tmp_dir, writer, config, profile,
    stats, keep_unzipped)`.
  - `addGeoJsonSource(name, path/glob)`: `GeoJsonReader::process(name, paths, writer, config, profile, stats)`.
  - `addParquetSource(name, paths, hive_partitioning, get_id, get_layer)`:
    `ParquetReader::with_options(name, stats, get_id, get_layer, hive_partitioning).process(&paths, writer, config,
    profile)` (filters files/row groups with `profile.cares_about`).
- `ParquetFeature` implements `SourceFeature` itself (owns a `SourceFeatureBase`) instead of being converted to a
  `SimpleFeature`: WKB/WKT decoding stays lazy so features a profile rejects by tags never parse their geometry, and
  nothing is copied. `vector_tile_feature_id` now uses `wrapping_mul` (Java `long` overflow semantics; parquet IDs are
  64-bit hashes). TODO(port): Java's `ParquetReader` feeds features through a `Distributor` so idle threads help
  with large row groups; here a thread processes a whole block.
- `ShapefileReader::new(projection, source, path)` / `with_bounds(.., &Bounds)`: self-contained `.shp`/`.dbf`/`.prj`/
  `.cpg` parser (no GeoTools). Inputs: `.shp` on disk, a path inside a zip (`zip/dir/x.shp`, from
  `file_utils::walk_path_with_pattern`) or a zip containing one shapefile; zip entries are streamed (not extracted)
  through `util::zip_entry_reader` (see below).
  One thread reads records, `available_parallelism` threads decode + reproject, output stays in file order; IDs
  start at 1 and skip null shapes like Java. Geometries match GeoTools: points, `MultiPoint`, `MultiLineString`,
  `MultiPolygon` (CW shells, CCW holes assigned to the smallest containing shell). DBF dates become `YYYY-MM-DD`
  strings; blank numerics are null; charset from `.cpg` (UTF-8, otherwise ISO-8859-1). Bounds filter uses the
  record bbox in the source CRS. `geo::crs` now converts WKT `PROJCS` (transverse mercator, LCC, albers, ...) to
  proj strings, so e.g. SWEREF99 `.prj` files work.
- `GeoPackageReader::new(projection, source, path, tmp_dir, keep_unzipped, Option<&Bounds>)`: rusqlite +
  GeoPackageBinary header parsing (`parse_geometry_header`) + WKB. SRS from `gpkg_spatial_ref_sys` WKT (falls back
  to `organization`/`organization_coordsys_id` EPSG codes), input forced longitude-first like Java. R-tree
  (`rtree_<table>_<col>`) used when bounds are given. Empty-flagged geometries are skipped. Declared `BOOLEAN`
  columns -> bool, `FLOAT` -> `Value::Float`, other columns by sqlite storage class; the `fid` column is a tag.
- `NaturalEarthReader::new(source, path, tmp_dir, keep_unzipped)`: zip inputs are extracted
  (`simple_reader::extract_from_zip`, Java's URL-encoded name when `keep_unzipped`, otherwise a temp file deleted on
  drop). Tables matching `ne_[a-z0-9_]+` are read **in parallel** (one read-only connection per thread); IDs are
  precomputed per table so they match Java's sequential IDs, but features from different tables interleave.
  Connections (also GeoPackage's) are opened by `natural_earth_reader::open_sqlite_for_parallel_reads`: `PRAGMA
  mmap_size` plus `SQLITE_CONFIG_MEMSTATUS=0`, because the bundled sqlite is built with
  `SQLITE_ENABLE_MEMORY_MANAGEMENT` (one page cache shared by all connections behind a global mutex) and the table
  threads otherwise spend most of their time contending on it and on the memstatus malloc mutex.
- Zip entries (Rust-only, no Java equivalent): `util::zip_entry_reader::ZipEntryReader` reads a stored/deflated entry
  straight from the zip file (memory-mapped) and checks its CRC-32 and size at the end. Deflated entries of at least
  16 MB compressed are inflated by `util::parallel_inflate::ParallelInflater` on `available_parallelism` threads
  (rapidgzip/pugz approach: 1 MB chunks, each worker finds the first dynamic-Huffman block header in its chunk and
  decodes with 16-bit markers for the unknown 32 KB window; the consumer checks that chunks line up, resolves the
  markers, and decodes a chunk sequentially itself when a guess was wrong, so a false positive costs time, not
  correctness). Smaller entries are inflated by zlib-rs on a read-ahead thread (`ReadAhead`).
  `open_with_read_ahead` picks one; `extract_from_zip` and the shapefile reader use it. The water polygons
  `.shp` (931 MB -> 1.27 GB) inflates in ~0.35s instead of ~2.4s.
- GeoJSON: `geojson::geo_json::GeoJson::{from_path, from_bytes, from_str, from_supplier}` with `iter()` (streaming
  `Iterator<Item = Result<GeoJsonFeature>>`), `features()`, `count()`. Hand-rolled pull tokenizer
  (`geo_json_feature_iterator::JsonParser`, Jackson token model + `skip_children`) so the Java state machine is
  ported 1:1 (same warnings/empty geometry rules). `GeoJsonReader::new(source, path)` (layer = file stem, IDs from 0).
- Parquet: uses `parquet`'s arrow reader (row group = `Block`, read in parallel by `ParquetReader`). Rows convert to
  `Tags` (`parquet_record_converter::array_value`): nulls omitted, structs -> maps, map keys stringified, legacy
  `array`/`list` 2-level lists unwrapped, float -> double, decimals -> double. Temporal values have no `Value`
  variant so they become their Java `toString()`: dates `2000-01-01`, times `05:30:10.123`, timestamps
  (millis) `2000-01-01T10:30:10.123Z`, UUID strings, `Interval[period=P1M2D, duration=PT3S]` (INTERVAL columns are
  re-read with the low-level column reader since arrow drops the month part). GeoArrow geometries are decoded
  straight from arrow arrays (`geo_arrow::GeoArrowColumn`); WKB/WKT lazily (`ParquetFeature::lat_lon_geometry`).
  parquet-mr `FilterPredicate` -> `geo_parquet_metadata::FilterPredicate` (numeric compare/and/or/not) used for row
  group pruning via statistics (`can_drop`) and row filtering (`evaluate`); IDs use the same fnv1a hashes as Java.
  `ParquetFeature` implements `WithTags` and has `into_reader_feature()`; TODO(port): `impl SourceFeature`.
### OSM reader notes (`reader::osm::osm_reader`, `osm_phaser`)

How the `Planetiler` orchestrator drives it (Java `Planetiler.addOsmSource`):
```rust
let node_locations = long_long_map::from_config(&config.node_map_type, &config.node_map_storage, &node_db_path,
    config.node_map_madvise)?;                                   // Box<dyn LongLongMap>
let multipolygons = long_long_multimap::new_replaceable_multimap_from_config(
    &config.multipolygon_geometry_storage, &multipolygon_path, config.multipolygon_geometry_madvise)?;
let reader = OsmReader::new(name, osm_input_file.get()?, Arc::from(node_locations), Box::new(multipolygons),
    profile.clone() /* Arc<dyn Profile> */, stats.clone());
reader.pass1(&config)?;                    // &PlanetilerConfig
reader.pass2(&feature_group, &config)?;    // &Arc<FeatureGroup>, &Arc<PlanetilerConfig>; also calls profile.finish
reader.close();                            // drops node locations / relation info / multipolygon geometries
```
(`OsmReader::new_in_memory_multipolygons` uses an in-memory multimap.) Static helpers:
`OsmReader::estimate_node_location_usage[_str]`, `estimate_multipolygon_geometry_usage[_str]` (return
`ResourceUsage`), `osm_reader::{can_be_line, can_be_polygon}`.

- Pass 1: with a parallel-write node map (`array`) each of `threads<8 ? threads : threads-1` workers decodes and
  processes its own blocks. With a sequential map (`sparsearray`, `sortedtable`; the default) `threads-2` parse
  threads decode blocks, **call `preprocess_osm_node` and encode node locations in parallel** (Java does both on the
  single writer thread; ordering vs. ways is preserved because the writer consumes blocks in order) and hand
  `(id, location)` pairs plus the remaining elements over a per-block channel to the single writer thread. Up to
  `8 * parse threads` parsed blocks can wait for the writer (a slow block at the head no longer stalls the reader),
  and mmap append stores write through a background I/O thread (`util::async_file_writer`).
  Profile panics in `preprocess_*`/`process_feature`/`finish` are caught and logged like Java exceptions.
- Relation info (`HashMap<i64, Arc<dyn OsmRelationInfo>>`, last info per relation wins like Java), way->relation and
  relation->parent memberships (sorted `Vec<(i64, i64)>`, role id in the top 16 bits) and the roaring set of
  multipolygon way ids are built under a lock in pass 1 and frozen on first pass-2 read (`SealOnRead`).
- Pass 2 features are `OsmSourceFeature` (one type, `OsmFeatureKind::{Node, Way, FullWay, SplitWay{unique_id},
  Multipolygon}`): owns the decoded element (tags are served from the element, `base().tags` is empty), geometry is
  computed lazily via the per-thread `Arc<NodeLocations>` (`OsmReader::new_node_location_provider()`); closed ways
  that are both line and polygon do one node-location lookup (line = polygon exterior). `original_element()`,
  `is_full_way()`, `is_split_way()`, `unique_id()`, `vector_tile_feature_id` (x10 + 1/2/3, split ways renumber
  to `id + segment * 10^k`) match Java. Test hooks: `process_pass1_block[s]`, `process_node_pass2`,
  `process_way_pass2`, `process_relation_pass2`, `split_way_if_necessary`.
- `GeometryCache` is now allocated lazily (boxed on first use) so creating/moving source features that never ask for
  derived geometries is cheap.
- `OsmPhaser`: mutex+condvar port of Java's `Phaser` usage; per-worker counts are plain atomic stores (no RMW per
  element); `arrive`/`arrive_and_wait_for_others`/`close` return `anyhow::Result` (out-of-order elements);
  dropping a `ForWorker` that did not reach `Done` closes it so other workers don't deadlock on errors.
- `Tags` is `IndexMap<String, Value, TagsHasher>` (foldhash with a per-process random seed): SipHash was ~6% of OSM
  decoding and every `tags.get(..)` in profiles. Create maps with `Tags::default()` /
  `value::tags_with_capacity(n)` (`IndexMap::new()` only exists for std's hasher).
- Binaries (`planetiler`, all benchmarks) use mimalloc as the global allocator (default `mimalloc` cargo feature;
  `--no-default-features` for the system allocator). With macOS's allocator, the per-thread `Arc<str>` strings the
  PBF decoder hands out (user names, tag values; refcounted once per element) sometimes share 128-byte cache lines with
  other threads' allocations, and the resulting false sharing made multi-threaded decoding run at ~40% IPC in most
  processes (see bench/RESULTS.md history). Library users should pick a thread-caching allocator too.
- Benchmarks: `cargo run --release -p planetiler-benchmarks --bin benchmark_osm_reader -- --osm_path=... 
  --profile=null|emit` and `java -cp planetiler-dist-*-with-deps.jar bench/java/BenchmarkOsmReader.java
  --osm_path=... --profile=null|emit` (same profiles, same storage defaults, print `RESULT pass1/pass2` wall/CPU).

### Archive writer notes (TileArchiveWriter)

How the `Planetiler` orchestrator runs the archive stage (Java `TileArchiveWriter.writeOutput`):
```rust
feature_group.prepare()?;                                  // optional, write_output prepares if needed
let mut archive = tile_archives::new_writer(&output_config, &config)?;   // Box<dyn WriteableTileArchive>
tile_archive_writer::write_output(
    &mut feature_group,                                    // &mut FeatureGroup
    archive.as_mut(),                                      // &mut dyn WriteableTileArchive
    Arc::new(output_size),                                 // Arc<dyn DiskBacked + Send + Sync>, progress logs only
    &TileArchiveMetadata::from_profile(&*profile, &config),
    layer_stats_path.as_deref(),                           // Some(path) required when config.output_layer_stats
    &config,                                               // &PlanetilerConfig
    &stats,                                                // &StatsRef
)?;                                                        // starts/stops the "archive" stage timer
archive.close()?;                                          // caller closes (Java try-with-resources)
```
`write_output` calls `initialize()`, writes all tiles, then `finish(metadata.with_layer_stats(..))`. Threads are
scoped (`archive_read-1`/`archive_merge-1`, `archive_encode-N`, `archive_write-N`, `archive_stats-1`, plus `read-N`
from `FeatureGroup::parallel_iterator` when `feature_read_threads > 1`) and registered for per-thread CPU logging; queue
and counter names match Java (`archive_reader_queue`, `archive_writer_queue`, `archive_layerstats_queue`,
`archive_tiles_written{zoom}`, `archive_memoized_tiles`, `archive_features_processed`). Each batch's result goes from
the encoder to the writer through a one-shot channel (Java's `CompletableFuture`), so writers consume batches in
order. Any failure aborts every queue; `UNKNOWN` compression returns an error up front. MVT encoding uses
`VectorTile::encode_with_layer_stats` (same stats as `tile_size_stats::compute_tile_stats` without building prost
structs), `--tile-format=mlt` uses `VectorTile::encode_mlt_with_layer_stats` (see "MLT output notes"), and tiles are
compressed with `util::gzip::gzip` (thread-local libdeflate, level 6; bytes differ from Java's zlib but sizes are
within ~0.5%). `TileWeights` implements `TileWeightSource`.

**Testing harness** (`planetiler_core::testing`, not cfg-gated): `run_with_reader_features(&[("threads", "1")],
vec![SimpleFeature..], |source, features| { .. })` (and `_and_post_process`, `_profile`, generic `run(args, |ctx|
process_reader_features(ctx, &features), Arc<dyn Profile>)`) returns `PlanetilerResults { tiles: BTreeMap<TileCoord,
Vec<ComparableFeature>>, metadata, tile_data_count }`. Expected features: `feature(geom, tags!{..})`,
`feature_in_layer`, `feature_with_id`, `ComparableFeature::new(GeometryComparison::{Norm,Round,Exact,Topo}(..), ..)`;
compare with `assert_submap(&tile_map([((x, y, z), vec![..])]), &results.tiles)`. `TestProfile` builds a profile from
closures. For OSM tests, a runner can drive the OSM reader into `ctx.feature_group` once it is ported.

**Validators.** `base_schema_validator::validate(&dyn Profile, &SchemaSpecification, &config) -> ValidationResult`,
`assert_valid(profile, &spec)` / `assert_valid_str` for tests (Java `TestUtils.validateProfile`),
`SchemaSpecification::load(path)` / `load_str`, `java_profile_validator::ProfileValidator` (CLI + `--watch`).
### MLT output notes (`mlt`, `--tile-format=mlt`)

**Decision: port the Java encoder (`org.maplibre:mlt` 0.0.10) instead of using the `mlt-core` crate.** The official
Rust crate `mlt-core` 0.21.0 (same repo, MIT OR Apache-2.0) was evaluated first. It can encode: FSST (the `fsst-rs`
crate), FastPFOR (the `fastpfor` crate), shared dictionaries, earcut tessellation, Morton/Hilbert/ID sorting, and
property type inference. Its tag `0x01` format is still compatible with Java 0.0.10: it decodes everything this
port (and Java) writes, see below. But it is a different encoder, so it cannot produce the tiles Java Planetiler
produces:

- It is a cost-based optimizer. The encoding of each stream is chosen by its own heuristics and options are "allow"
  flags (`EncoderConfig::with_fsst`, `with_fastpfor`, `with_shared_dict`, `Tessellation::{Off, Auto, Always}`).
  Java's choices are fixed rules: always the smallest of plain / delta / RLE / delta-RLE, a forced RLE for constant
  streams, plain vs. Hilbert vertex dictionary vs. Morton dictionary by encoded size, string plain vs. dictionary
  vs. FSST dictionary, shared dictionaries only for columns Planetiler's `findStringColumns` picks. Its FSST symbol
  tables come from a different implementation. The bytes, and so the tile sizes, would differ from Java everywhere.
- Some Java behavior can't be expressed. Java's `COERCE` type mismatch policy turns a column with mixed types into
  strings (`PlanetilerTests.testAttributeTypeCoercion` expects `"1"`, `"1.5"`, `"true"`), while mlt-core widens
  ints and floats to `F64`. Java's tessellation without `--mlt-polygon-outline` writes triangles with no outline
  topology, which mlt-core only supports in its v2 format. Java's planetiler-specific options (`mlt_reorder_features`
  sorts by ID only when IDs are kept, `mlt_shared_dict` picks columns per tile) would need re-implementing anyway.
- It would add about 25 crates (including the `hotpath` profiling macros, `wide`, `probabilistic-collections`,
  `union-find`) for a format the port can produce byte-for-byte from about 3k lines of ported code.

The port mirrors the rest of the project, where Java output parity is the acceptance test. Exact byte sizes
(`TileSizeStatsTest`'s 85/41/25 and 95/50 layer bytes) and the Java tests port directly. mlt-core is used only as an
independent decoder to check the output (`bench/mlt-core-check`, kept out of the workspace so it adds no
dependencies). A hybrid (port the planetiler-side logic, encode with mlt-core) was rejected for the same
byte-parity reasons.

**What is ported** (`crates/planetiler-core/src/mlt/`, from maplibre-tile-spec tag `java-v0.0.10`): `MltConverter`
(`create_tileset_metadata`, `convert_mvt`), `ConversionConfig` / `FeatureTableOptimizations` / `ColumnMapping`
(explicit names or literal prefix + delimiter; the mapping config applies to all layers like Planetiler's
`MATCH_ALL`), `IntegerEncoder` (`AUTO` only), `StringEncoder`, `PropertyEncoder`, `GeometryEncoder` (plain /
dictionary / Morton vertex buffers, pre-tessellation with and without outlines), `Hilbert` / `ZOrderCurve`,
`StreamMetadata` (+ RLE / Morton), `MltTypeMap.Tag0x01`, embedded layer metadata, byte / boolean RLE. The
third-party codecs are ported too: `fastpfor` is JavaFastPFOR 0.3.10 `Composition(FastPFOR, VariableByte)` (block 256,
words big-endian), `fsst` is the pure-Java `SymbolTableBuilder` (MLT uses it unless the JNI library is loaded), and
`earcut` is earcut4j 3.0.0. Java quirks are kept on purpose because they change the bytes: the Hilbert "dictionary" is
never deduplicated (the `last` variable is never updated), FSST symbol hashes include truncated bytes, and the FSST
candidate order follows `java.util.HashMap` iteration and `PriorityQueue` heap order. `fsst` reproduces both (the map's
order is derived from its final table size: by bin, newest key first; see `fsst::Candidates`); bins that
Java would convert to trees (8+ colliding keys in a 64+ bucket table) are counted in `fsst::TREEIFIED_BINS` but
their order isn't emulated. That never happened on Switzerland. `mlt::encode_tile` is Java `TileArchiveWriter`'s MLT
branch (`findStringColumns`, `COERCE`, per-layer optimizations, Morton on, `ALL` outlines), and
`VectorTile::{to_mlt_input, encode_mlt, encode_mlt_with_layer_stats}` are `toMltInput` + encode +
`TileSizeStats.computeMltTileStats`. Archive metadata `format` is `application/vnd.maplibre-tile`, and PMTiles tile
type 6 (both already ported).

**Decoder.** `mlt::decoder` decodes what the encoder writes (like `MltDecoder.decodeMlTile`). It is used by the test
harness (`testing::get_tile_map_with_format`, MLT geometries scaled by 256 / extent like Java `TestUtils`),
`tile_size_stats::compute_mlt_tile_stats` (`consume_column` is Java's stream-skipping logic) and `compare`, which now
also compares MLT archives (converted to MVT protobufs, then the same normalized comparison; Java's
`CompareArchives` reads MVT only). Polygons tessellated without outlines can't be decoded. Java's decoder and
mlt-core can't decode them either (mlt-core: "v1 triangles without outline topology").

**Not ported / differences**: `IntegerEncodingOption`s other than `AUTO`, layer filter patterns, the remote
tessellation service, and geometry sorting with ID regeneration (`SortSettings.isSortable`, never set by
Planetiler: it needs `mlt_reorder_features` with IDs excluded but only sorts when IDs are included; requesting it
returns an error). List / map / bytes attribute values become strings, as MVT does (Java's encoder throws).
`TileSizeStats.main` (`stats`) still reads MVT only, like Java.

**Verification.**
- Performance rewrites keep the straight ports of `fsst` and `integer_encoder` as test-only references
  (`fsst_reference.rs`, `integer_encoder_reference.rs`) with differential tests; the encoder reads features through
  the `MltFeature` trait (`VectorTile::encode_mlt*` pass borrowed attributes). Details in bench/RESULTS.md.
- Unit tests: round-trip encode/decode over all option combinations (`mlt::tests`), the ported `TileSizeStatsTest`
  MLT cases with Java's exact byte counts plus the `fastpfor.mlt` fixture Java wrote, and `PlanetilerTests`
  (`testSinglePoint` MLT variants, `testAttributeTypeCoercion`, `testPlanetilerRunner` with `--tile-format=mlt`, all
  `mlt_*` options, pmtiles and files outputs).
- Byte identity with Java. `bench/java/MltReference.java encode` runs Java's exact `TileArchiveWriter` MLT code on
  every tile of an MVT mbtiles, and `mlt_reference encode` (planetiler-benchmarks) does the same in Rust.
  `mlt_reference compare` diffs the two (`mlt_reference bench <mbtiles> <step> [options]` times only the encoding). On the Java OpenMapTiles outputs for Monaco (23 tiles, 5,357 features) and
  Switzerland (23,922 tiles, 5.6M features), every tile is **byte-identical** for: default, `exclude_ids`,
  `fastpfor`, `fsst`, `fsst`+`fastpfor` (`mlt_advanced`), `shared_dict`, `reorder_features`, `tessellate`,
  `tessellate`+`outline`, `fsst`+`fastpfor`+`shared_dict`, and everything at once. `mlt_reference encode` also
  checks that decoding each tile gives back the MVT features. Uncompressed Switzerland totals (same on both sides):
  default 457.7 MB, `exclude_ids` 436.5 MB, `fastpfor` 453.3 MB, `fsst` 444.4 MB, `mlt_advanced` 440.4 MB,
  `shared_dict` 379.2 MB, `fsst`+`fastpfor`+`shared_dict` 370.0 MB, `tessellate`+`outline` 639.8 MB.
- End to end, Monaco with Java `planetiler-dist` vs Rust `planetiler`, `--tile_format=mlt` and also
  `--mlt_advanced=true`: `planetiler compare` finds equivalent normalized content in all 23 tiles. 3 of 23 are
  byte-identical (uncompressed). The other 20 differ only because the two pipelines' *MVT-level* tiles already
  differ in ways `compare` normalizes away (ring start points, tag order; MVT bytes differ in all 23 tiles).
  Each side's e2e MLT is byte-identical to that side's MVT output re-encoded by `mlt_reference`. Sizes
  (uncompressed / gzipped): default Java 431,652 / 218,054 vs Rust 431,640 / 216,549 bytes; advanced Java
  391,862 / 262,729 vs Rust 391,881 / 262,076. Gzip differences are libdeflate vs zlib.
- Decoders. Java's `MltDecoder` and `mlt::decoder` give identical features (WKT, IDs, typed properties) on the Rust
  outputs. mlt-core 0.21 decodes every Monaco variant (except tessellated-without-outline) to the same features as
  `mlt::decoder` (`bench/mlt-core-check`).

### planetiler-openmaptiles notes

**Generated code.** `Generate.java` -> `planetiler_openmaptiles::generate` (parsing/expression/emit functions, ported
`GenerateTest`) + the `generate_openmaptiles` binary. Instead of crawling GitHub, it reads a vendored copy of the
schema in `crates/planetiler-openmaptiles/schema` (pinned to openmaptiles `e0fb503`, the revision the checked-in Java
`generated/*.java` match; refresh with `crates/planetiler-openmaptiles/scripts/fetch-schema.sh <ref> <dir>`), writes
`src/generated/{open_map_tiles_schema,tables}.rs` and runs `rustfmt`. `generate::tests::test_generated_code_is_up_to_date`
fails if the checked-in files drift from the generator output. Differences from Java: docs are markdown doc comments
(no HTML rendering); YAML is parsed with `serde_yaml` (YAML 1.2: unquoted `yes`/`no` stay strings, the pinned schema
quotes them).
- `OpenMapTilesSchema.<Layer>` -> module `open_map_tiles_schema::<layer>` with `LAYER_NAME`, `BUFFER_SIZE`,
  `fields::*`, `field_values::*` (`*_VALUES` are `&[&str]`) and `field_mappings::*` (`LazyLock<MultiExpression<&'static
  str>>`, call `.index()` once). `create_instances(&Arc<Translations>, &Arc<PlanetilerConfig>, &StatsRef) ->
  Vec<Arc<dyn Layer>>`.
- `Tables.OsmX` records -> `tables::OsmX<'a> { source, mapping_key }`: a lazy, allocation-free view; column accessors
  come from the generated `WithX<'a>` traits (`Option<Cow<'a, str>>` for strings, `bool`, `i64`, `i32`, `&'a str` for
  the mapping key, `&'a dyn SourceFeature` for `source()`), computed from tags on each call. `OsmX::mapping()` is the
  imposm3 `MAPPING` expression. `Tables.OsmX.Handler` -> trait `OsmXHandler { fn process_osm_x(&self, &OsmX<'_>,
  &mut FeatureCollector<'_>) }`; layers expose it through `RowHandlers::as_osm_x_handler()` (Java `instanceof`).
  `RowClassAndConstructor`/`generateDispatchMap` -> `enum Table` (`ALL`, `name`, `mapping`, `is_handled_by`,
  `dispatch`) and `tables::MAPPINGS: MultiExpression<Table>`.

**Layers / profile.** `Layer` (crate root `layer` module) is `Handler + RowHandlers + Any`, plus accessors for the
OpenMapTiles source processors (`as_natural_earth_processor`, `as_lake_centerline_processor`,
`as_osm_water_polygon_processor`, `as_osm_all_processor`) and `ignore_wikidata()` (Java `IgnoreWikidata`). Those
processor traits live in `open_map_tiles_profile`. `OpenMapTilesProfile::new(Arc<Translations>, Arc<PlanetilerConfig>,
StatsRef)` embeds a `ForwardingProfile` (with the `transportation_name -> transportation` layer dependency), registers
layers that pass `--only-layers`/`--exclude-layers`, wires `TransportationName::needs_transportation_layer`, registers
per-source closures, and builds one `MultiExpression<RowDispatch>` index over all imposm3 tables (tables nobody
handles are dropped) plus a `bool` index for `cares_about_wikidata_translation` (evaluated on a borrowed
tags+geometry-type view instead of building a `SimpleFeature`). `ExtraLayers` -> `addons::create`.

**Main.** `OpenMapTilesMain.run` -> `open_map_tiles_main::setup(&Arguments) -> OpenMapTilesSetup` (default languages,
`wikidata_names.json` path, the four `SourceSpec`s with default paths/URLs, `mbtiles` output) and `create_profile(..)`;
`configure(&Arguments) -> Planetiler` applies it to the runner (profile built lazily with `set_profile_provider` from
`p.translations()`), `run`/`main` run it. Sources: `lake_centerlines` (shapefile, EPSG:3857,
`data/sources/lake_centerline.shp.zip`, `https://dev.maptiler.download/geodata/omt/lake_centerline.shp.zip`),
`water_polygons` (shapefile, `data/sources/water-polygons-split-3857.zip`,
`https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip`), `natural_earth` (sqlite,
`data/sources/natural_earth_vector.sqlite.zip`, `https://dev.maptiler.download/geodata/omt/natural_earth_vector.sqlite.zip`),
`osm` (`data/sources/<area>.osm.pbf`, `geofabrik:<area>` or `aws:latest` for `planet`; `--area` defaults to `monaco`).

**Utilities.** `OmtLanguageUtils.getNames(tags, translations)` -> `omt_language_utils::get_names(&Tags,
Option<&Translations>) -> Tags`; `Utils.coalesce` is `Option::or`; `elevationTags` -> `utils::elevation_tags[_str]`;
`brunnel` -> `utils::brunnel[_with_ford]` (`Option<&'static str>`). `VerifyMonaco` uses
`Verify::{get_num_features, check_min_feature_count[_range]}` from `planetiler_core::mbtiles::verify`.

**FeatureMerge.** Until `planetiler_core::feature_merge` lands, the crate uses `src/feature_merge_shim.rs`, a copy
of that port with the same API (`TODO(port)`: replace with `pub(crate) use planetiler_core::feature_merge;` in
`lib.rs` and drop the `rstar`/`hashbrown`/`ahash` dependencies).

**Tests.** `AbstractLayerTest` -> `test_support` (`LayerTest`, `assert_features`, `assert_covers_zoom_range`,
feature constructors, `to_map`/`assert_submap` copies of the core test helpers, `SharedWikidataTranslations` so tests
can add wikidata translations after building `Translations`). Per-layer tests live in each layer file.
`tests/open_map_tiles_test.rs` ports `OpenMapTilesTest` (Monaco end-to-end, all counts match Java).

### Planetiler orchestrator notes (`planetiler::Planetiler`, CLI, examples)

```rust
let mut p = Planetiler::create(arguments)?;             // reads tmpdir/temp_* paths, --download, --only_download, ...
p.set_default_languages(&LANGS)
    .fetch_wikidata_name_translations("data/sources/wikidata_names.json")?
    .set_profile_provider(|p| Ok(Arc::new(MyProfile::new(p.translations(), p.config().clone(), p.stats().clone()))))
    .add_osm_source_with_url("osm", "data/sources/monaco.osm.pbf", Some("geofabrik:monaco"))?
    .add_shapefile_source_with_projection(Some("EPSG:3857"), "lakes", "lakes.shp.zip", Some(url))?
    .add_natural_earth_source("ne", "ne.sqlite.zip", Some(url))?
    .add_geopackage_source(None, "gpkg", "x.gpkg", None)?
    .add_geojson_source("gj", "x.geojson", None)?
    .add_parquet_source("pq", paths, hive_partitioning, get_id, get_layer)?   // Option<Arc<dyn Fn(&Tags) -> Value>>
    .add_overture_source("overture", "buildings", "building", "data/overture")?
    .add_stage("custom", "description", |ctx: &mut StageContext| Ok(()))?      // also add_source_stage (cares_about_source)
    .overwrite_output("data/output.mbtiles")?;          // or set_output / set_output_with_argument("mbtiles", ..)
p.run()?;
```
- Builder methods that read arguments or can fail return `anyhow::Result<&mut Self>`; Java overloads became
  `_with_url` / `_with_projection` variants or `Option` parameters. `set_profile(impl Profile)`, `set_profile_arc`,
  `set_profile_provider(FnOnce(&mut Planetiler) -> Result<Arc<dyn Profile>>)` (constructed at `run()` like Java).
- Stage tasks are `FnOnce(&mut StageContext) -> Result<()>`; `StageContext { name, feature_group: &Arc<FeatureGroup>,
  profile, config: &Arc<PlanetilerConfig>, stats, metadata: &mut TileArchiveMetadata, tmp_dir }`. Stage errors are
  wrapped with `Error occurred during stage <id>`. After the stages the group is reclaimed with `Arc::into_inner`,
  prepared, and (with `--reuse_featuredb`) its string encoders/chunk manifest are saved under `feature.db/`.
- `run()` mirrors Java: version/help, `--tests=<yml>` (`java_profile_validator::validate`; returns an error when a
  case fails instead of exiting 1), output exists/`--force`/`--append` checks, multi-writer index files, cleanup of
  tmp files, downloads (lazy overture STAC resolution on a background thread), `download_osm_tile_weights`, input
  existence, wikidata fetch/load, disk + memory checks (`ResourceUsage`, fail unless `--force`), bounds from the OSM
  header then `OsmNodeBoundsProvider` (`--osm_parse_node_bounds` skips the header), stages, `free_<name>_after_read`,
  `TileArchiveWriter`, summary. `--help`/`--version` return `Ok(())` (Java calls `System.exit(0)`).
- `PlanetilerConfig` is an `Arc`; inferred bounds produce a *new* config Arc (profiles built before that keep the old
  one; everything the runner drives uses the new one). `Bounds::lat_lon_opt()` was added for that check.
- Wikidata translations are attached through a deferred provider registered by `translations()` (a `OnceLock`
  filled during `run()`), since `Arc<Translations>` is shared with the profile.
- Not modelled: `LogUtil.setStage` prefixes; virtual-thread executor. (The write-phase disk estimate subtracts
  `free_<name>_after_read` inputs like Java, via `ResourceUsage::free_disk`.)
- Behaviour fixes made while comparing against Java on Monaco/Switzerland: `polygon_overlay::reduce_precision` now
  snap-rounds like JTS's `SnapRoundingNoder` (`geo::snap_rounding`), so sub-pixel slivers collapse like JTS; `FeatureGroup` writes a tile
  with an empty layer (like Java) when all of a layer's features are removed by `unscale/removePointsOutsideBuffer`;
  `test_utils::validate_geometry` uses JTS `isSimple` semantics like Java's `TestUtils`; custommap reads OSM tags via
  `WithTags::tags` (OSM features have empty `base().tags`); `extract_from_zip` handles relative tmp dirs.
- CLI (`crates/planetiler/src/main.rs`): `planetiler [task] [args]`, tasks as in `Main.java` (`openmaptiles` default
  with the maptiler URL overrides, `custom`, `shortbread`, `*.yml` first arg, `example-*`, `overture`, `osm-qa`,
  `verify`/`verify-schema`, `verify-mbtiles`, `verify-monaco`, `stats`, `compare`). Logging via `tracing-subscriber` (`RUST_LOG`), messages
  written verbatim so progress-log ANSI colors render.
- Examples: `planetiler_examples::<module>::{main(args), run(..)}` for `toilets_overlay`, `toilets_overlay_low_level_api`,
  `bike_route_overlay`, `osm_qa_tiles`, `admin_borders_overlay`, `parks_overlay`, `tram_route_overlay`,
  `water_infrastructure_project`, `overture`.

**Comparison against Java** (`planetiler-dist-0.10.3-SNAPSHOT`, decoded tiles compared ignoring polygon ring start
point/order and feature order; now `planetiler compare [--ignore-ids] java.mbtiles rust.mbtiles`, the numbers below
came from equivalent Python scripts; current Switzerland numbers are in `docs/switzerland-parity.md`):
- Toilets overlay (Monaco, `--process_threads=1` because its sort key is a processing-order counter): identical.
- `custom --schema=owg_simple.yml` (Monaco): identical.
- OpenMapTiles Monaco: identical. (The 4 z13 merged `building` features that used to differ match since the JTS
  buffer port.)
- OpenMapTiles Switzerland (default threads): same feature count in every layer; differences: 2791 z13 merged
  buildings + 725 z6-12 `landuse` + 21 `landcover` (all the same buffer-merge vertex difference), 10 `transportation`
  lines (line-merge picks a different continuation at a junction / 1-unit vertex shifts), 1730 housenumber merged-point
  IDs (multi-threaded input order), 28 empty tiles Java writes and Rust skipped (OMT `mountain_peak` post-processing
  nulls every feature; since fixed: dropped peaks now get an empty geometry so the empty layer is kept), and lower
  `osm_boundary_missing_way`/`osm_multipolygon_missing_way`/`merge_snap_fix_input` data error counts (Java recomputes
  and re-logs failed geometries per layer).
  Re-run 2026-10-18 (`docs/switzerland-parity.md`): tile sets identical (the 28 empty tiles are fixed); 2387 z13
  buildings, 404 landuse, 21 landcover, 10 transportation, 1730 housenumber ID diffs; one extra z13 building in Rust
  (a merged polygon of area 4.002 px² at the 4 px² merge threshold).
  After the JTS buffer port: same feature count in every layer; 578 z13 buildings, 35 landuse, 17 landcover and 10
  transportation features differ (`docs/switzerland-parity.md`); unchanged in the `627d901` benchmark pass, where
  Monaco is identical to Java including feature IDs. After the snap-rounding port (`geo::snap_rounding`, also used by
  the buffer's fixed precision fallback) and the `buffer_zero` collinear-node fix: 563 buildings, 31 landuse,
  0 landcover, 10 transportation; Monaco still identical.
- Fixes made while comparing: JTS snap rounding in `reduce_precision`, Java multimap order for relation
  memberships (`route_N_*` attributes), Java `HashMap` iteration order for OMT housenumber grouping, custommap OSM
  tags, empty layers after point-buffer filtering (see above).

## Follow-ups (tracked by the coordinator)

- Replace `planetiler-openmaptiles/src/feature_merge_shim.rs` with `planetiler_core::feature_merge` once FeatureMerge
  merges; drop the crate's `rstar`/`hashbrown`/`ahash` deps.
- Move `Verify.checkMinFeatureCount`/`getNumFeatures` from `planetiler-openmaptiles/src/util/verify_monaco.rs` to
  `planetiler_core::mbtiles::verify`.
- Unify the two `VectorLayer` types (`archive::tile_archive_metadata` and `util::layer_attr_stats`).
- Re-enable the four `#[cfg(any())]` merge tests in `crates/planetiler-core/tests/` once FeatureMerge merges.
- Port custommap `SchemaValidator` CLI, `TopOsmTiles.main` (xz), `OpenMapTilesMapping` benchmark; in-memory stream
- Port JTS `Polygonizer` into core and use it for OpenMapTiles boundary country polygons (currently a line-merge +
  union stand-in that drops rings closing only through a node shared by 3+ ways).
- Re-enable the four `#[cfg(any())]` merge tests in `crates/planetiler-core/tests/` once FeatureMerge merges.
- `feature_merge::buffer_geometry`: near-collinear mitre vertices are now removed (Monaco z13 buildings within ~1.5%
  of Java's vertex count); exact parity would need a port of the JTS offset-curve builder.
- Port `TopOsmTiles.main` (xz), `OpenMapTilesMapping` benchmark; in-memory stream
  archive readers for the csv/tsv/json/proto/pbf `testPlanetilerRunner` variants (including their MLT variants).
- MLT encoding speed: done (bench/RESULTS.md "MLT encoding": 6x Java on the benchmark tile, advanced archive
  stage 28.5s -> 5.1s). Remaining MLT cost is mostly the geometry dictionary candidates (curve codes, radix sort)
  and FSST table building; any further change must stay byte-identical (`mlt_reference` on Switzerland, 11 variants).
### custommap notes (`crates/planetiler-custommap`)

**Loading.** `SchemaConfig::load(path)` / `load_str(yaml)` go through `util::yaml` (anchors, aliases, `<<` merge keys)
then serde with `deny_unknown_fields` (Jackson's default). Java `Object` fields are `Option<Value>` (mapping keys become
strings like Jackson; YAML `null` == missing). `ConfiguredMapMain` -> `configured_map_main::{configure(&Arguments) ->
ConfiguredMap { root, profile: Arc<ConfiguredProfile>, sources: Vec<ConfiguredSource> }, load_schema, main}`;
`create_planetiler(&ConfiguredMap)` / `add_source` build the runner and `main` runs it (`tests/configured_map_test.rs`). Samples are bundled with `include_str!`
(`samples::get("shortbread.yml")`, also `/samples/...`), as are `planetiler.schema.json`/`planetilerspec.schema.json`.

**Contexts.** `contexts::build_root_context(&Arguments, schema.args()) -> Result<Root, ParseException>` iterates
argument definitions until they settle like Java. `Root` is a cheap `Arc` handle (argument values, `PlanetilerConfig`,
prebuilt CEL bindings for `args`/`args.<name>`). The nested Java context records are one `Copy` struct
`ScriptContext<'a>` tagged with a `ContextKind` (`Root < ProcessFeature < FeaturePostMatch < FeatureAttribute`):
`root.create_process_feature_context(&feature, &tvp)`, `.create_post_match_context(&match_keys)`,
`.create_attr_zoom_context(&value)`, `root.as_context()`. The input is a `FeatureRef` (a `&dyn SourceFeature`, or
`SplitTest(&Way)` for `split_osm_way_at_intersections` so no tags are cloned). `ScriptContext` implements
`ExpressionInput`; core custom expressions/getters recover it with `ScriptContext::from_input(&dyn ExpressionInput)`
(`as_any` returns a private `repr(transparent)` lifetime-erased wrapper that only `from_input` can downcast to,
see the SAFETY comment) and compare `kind()` to emulate Java's `instanceof`.

**Expressions.** `ScriptEnvironment { kind, root }` replaces the CEL declaration list (`contains_variable`).
`ConfigExpression` is an enum (`Const`, `Match(Arc<MatchExpression>)` with a prebuilt `MultiExpression` index,
`Coalesce`, `Variable`, `GetTag`, `GetArg`, `Cast`, `Script(Arc<ConfigExpressionScript>)`) implementing
`Simplifiable` like Java; Java's output `Class<O>` is `type_conversion::OutType` (`Integer`/`Long` both produce
`Value::Int`). `expr.into_getter()` wraps it as a core `TypedGetter` for `match_any_typed`.
`BooleanExpressionScript::script(text, &env) -> Expression::Custom`. `ParseException`/`EvaluationException` are error
structs; runtime script errors are logged once per script (then at debug) and evaluate to `null`/`false` instead of
throwing.

**CEL (`expression::stdlib`).** Uses the `cel` 0.15 crate: one shared `Env` (stdlib + strings extension + planetiler
functions `coalesce` (0..32 args), `nullif`, `replaceRegex`, map `has(key, values...)`/`get`/`getOrDefault`/
`getOrKeep`, `min`/`max` of lists, geometry `area(unit)`/`length(unit)`/`point_along_line`/`innermost_point`/
`partial_line` and fields `lat lon min_lat max_lat min_lon max_lon bbox centroid centroid_if_convex validated_polygon
point_on_surface line_midpoint innermost_point`). Scripts are parsed once into an AST and evaluated with
`cel::Value::resolve_val` against a `VariableResolver` over the `ScriptContext`; `feature.tags` is a lazy map `Val`
over the feature's tags (applies `tag_mappings`, `null` for missing keys, no per-feature map building). Static
scripts (only `args`) are folded to constants at parse time (cached per script). Gaps vs cel-java: no type checker
(undeclared variables are rejected at parse time by walking the AST, but type errors and unknown functions only
surface at runtime); `has(m.f)` is rewritten to an internal function so maps with null-for-missing work; `1 == 1.0`
is true (cel-java: false); `replaceRegex` uses `regex` crate syntax (no look-around/backrefs); geometry values
can't be returned as results (they become `null`). Geometry functions reach the borrowed source feature by indexing
the geometry value with an internal `"\0name\0args"` key since overloads can't downcast non-`'static` values.

**Profile.** `ConfiguredProfile::new(schema, root) -> Result<_, ParseException>` implements `Profile`
(`process_feature` uses a `MultiExpression` index over all features, `split_osm_way_at_intersections`, metadata);
`sources()` evaluates `url`/`local_path`/`projection` expressions; `find_feature_layer`.
`post_process_layer_features` uses `planetiler_core::feature_merge`. `configured_profile::can_be_line` ports
`OsmReader.canBeLine`. `validator::{validate(schema, spec), SchemaValidator, main}` ports `SchemaValidator` on top of
core's `BaseSchemaValidator` (`planetiler verify schema.yml [--watch]`). Tests:
`tests/{configured_feature_test, expression_tests, schema_tests}.rs` (the latter runs the full shortbread spec through
the core validator, `SchemaValidatorTest` including the CLI output, plus an ignored `bench_shortbread` throughput
check: ~0.9M features/s/thread in release).
- custommap CEL gaps vs cel-java (no type checker, `1 == 1.0` is true, Rust regex syntax in `replaceRegex`): consider
  closing these if real schemas hit them.
  `ParquetFeature` implements `SourceFeature` and has `into_reader_feature()`.
