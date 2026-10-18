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

## Status

See the table at the bottom of this file; update it as modules are completed.

| Area | Status |
|------|--------|
| Foundation types (`value`, `geo::{envelope, geometry_type, geometry_exception, tile_coord, tile_extents}`, `util::hilbert`, `stats` trait, `config` struct) | done |
| `vector_tile` (VectorTile, VectorGeometry, VectorGeometryMerger, Feature, encode/decode), `util::{tile_size_stats, layer_attr_stats, tileset_summary_statistics}` | done (MLT encoding/stats, `TileSizeStats.main` CLI deferred: see `TODO(port)`) |
| `reader::{structs, with_tags, with_geometry_type, json_conversion}` (dotted keys via `WithTags::get_tag_nested`; `get_tag` stays a direct lookup) | done (TODO: switch `with_tags::parse_shim` to `util::parse`; Struct has no Instant/LocalDate/UUID variants) |
| `reader::osm::{pbf_decoder, pbf_field_decoder, osm_element, osm_header, osm_input_file, osm_block_source, poly_file_reader}` (raw/zlib/lz4/zstd, eager or mmap lazy reads; hand-written zero-copy protobuf decoder, zlib via libdeflate) | done |
| `expression::{expression, multi_expression, data_type, simplifiable}` (input trait `ExpressionInput`) | done |
| `planetiler-benchmarks` `benchmark_osm_read` binary (emulates `OsmReader.pass1` until it is ported) | done |
| `collection` (append stores, `LongLongMap` + impls, `LongLongMultimap`, heaps, `LongMerger`, `IntRangeSet`, `SortableFeature`, `FeatureSort`, `ExternalMergeSort`) + `LongLongMapBench` (`planetiler-benchmarks` bin `long_long_map_bench`) | done |
| `config` (`Arguments`, `PlanetilerConfig::from`, `Bounds`) | done |
| `stats` (`Stats`, `InMemoryStats`, `PrometheusStats`, `DefaultStats`, `ProgressLoggers`, `ProcessInfo`, `Timers`, `Timer`, `ProcessTime`, `Counter`) | done (no GC/heap stats; per-thread CPU via thread registry) |
| `worker` (`Worker`, `WorkerPipeline`, `WorkQueue`, `WeightedHandoffQueue`, `Distributor`) | done (`processAndWrite` is `collection::feature_group::process_and_write`) |
| `util` (format, ansi_colors, parse, imposm3_parsers, var_int, hashing, sort_key, merging_range_map, sliding_window, zoom_function, cache_by_zoom, bin_pack, common_string_encoder, byte_buffer_util, gzip, yaml, build_info, madvise, memory_estimator, file_utils, file_watcher, resource_usage, tile_weights, downloader, aws_osm, geofabrik, overture_stac, top_osm_tiles) | done |
| benchmarks: `benchmark_var_int`, `benchmark_tile_coord` | done |
| Archives (`archive::{tile, tile_encoding_result, readable_tile_archive, writeable_tile_archive, tile_archive_config, tile_archive_metadata(_de_ser), tile_archives}`, `mbtiles`, `pmtiles`, `files`, `stream`, `util::compare_archives`, `geo::tile_order`) | done (`TileArchiveMetadata::from_profile`); deferred: `ArchiveOptions` stands in for the unported `Arguments` API, `VectorLayer`/`FieldType` live in `archive::tile_archive_metadata` until `util::layer_attr_stats` is ported, geometry-level checks in `mbtiles::{verify, compare}` / `CompareArchives` wait on `vector_tile` decoding |
| Benchmarks `benchmark_mbtiles_writer`, `benchmark_mbtiles_read`, `benchmark_pmtiles` (`crates/planetiler-benchmarks/src/bin`) | done |
| `geo` (geo_utils, simplifiers, smoothers, line_splitter, geometry_pipeline/transformer, point/polygon index, unit, with_geometry, tile_order, simplify_method) + JTS ports (`is_valid`, `polygon_overlay` fix/precision-reduce/boolean ops, `interior_point`/centroid, `maximum_inscribed_circle`, `normalize`, `precision_model`, `wkt`, `crs`) + `util::loop_line_merger` + `geo::test_utils` | done (see notes below) |
| `reader::osm::{osm_multipolygon, osm_way_splitter, osm_node_bounds_provider}` | done (`NodeLocationProvider` trait lives in `osm_multipolygon`, re-exported from `osm_reader`) |
| `reader::osm::{osm_reader, osm_phaser}` (+ `OsmReaderTest`, `OsmPhaserTest`, monaco end-to-end test matching Java feature counts / data errors), `benchmark_osm_reader` bin + `bench/java/BenchmarkOsmReader.java` | done (see notes below) |
| `util::{translations, language_utils, thread_local_transliterator, wikidata}` | done (`wikidata::fetch` takes a `Fn(&OsmElement) -> bool` filter; `fetch_for_profile` / `profile_filter` adapt a `Profile`) |
| Feature pipeline spine: `profile` (`Profile`, `NullProfile`), `feature_processor`, `forwarding_profile`, `feature_collector`, `reader::{source_feature, simple_feature}`, `reader::osm::osm_relation_info` (+ `RelationMember`), `render::{feature_renderer, tiled_geometry, geometry_coordinate_sequences, rendered_feature}`, `collection::feature_group` | done (see notes below) |
| `reader::{simple_reader, source_feature_processor, shapefile_reader, geo_package_reader, natural_earth_reader}`, `reader::geojson::*`, `reader::parquet::*` + benchmark `benchmark_simple_readers` | done as *decoders* emitting `ReaderFeature`; wiring into `SimpleFeature`/`Profile`/`FeatureGroup` is `TODO(port)` (see notes) |
| `archive::tile_archive_writer` (`TileArchiveWriter.writeOutput`), `validator::{schema_specification, base_schema_validator, java_profile_validator}`, `testing` harness + `tests/planetiler_tests*.rs` (the `runWithReaderFeatures` subset of `PlanetilerTests`), benchmark `benchmark_tile_archive_writer` | done (MLT output errors) |
| `planetiler` (`Planetiler.java` orchestrator) + `tests/planetiler_runner_tests.rs` (testPlanetilerRunner*, MemoryCheck*, ReuseFeatureDb) + `tests/planetiler_osm_tests.rs` (all OSM `PlanetilerTests`) + `testing::run_with_osm_elements`, `TileSizeStats.main` (`tile_size_stats::main`), `CompareArchives.main` (`compare_archives::main`) | done (see "Planetiler orchestrator notes"; deferred: csv/tsv/json/proto/MLT variants of testPlanetilerRunner, testExceptionWhileProcessingOsm is `#[ignore]`d) |
| `planetiler` CLI crate (`planetiler-dist` `Main.java` task dispatch) | done (stubs that error: `verify`/`verify-schema` (custommap SchemaValidator CLI), `benchmark-mapping`, `benchmark-longlongmap` (use the benchmarks crate), `top-osm-tiles` (needs xz)) |
| `planetiler-examples` (all 9 examples incl. `ToiletsOverlayLowLevelApi`, `OvertureBasemap`) + all example tests | done |
| `planetiler-openmaptiles`: `Generate` (generator + vendored schema), generated `OpenMapTilesSchema`/`Tables`, `OpenMapTilesProfile`, `OpenMapTilesMain` (`setup`/`configure`/`run`/`main`), all 16 layers, `OmtLanguageUtils`, `Utils`, `VerifyMonaco`, all unit tests (249), `OpenMapTilesTest` (`tests/open_map_tiles_test.rs`) | done (see notes below) |
| `planetiler-custommap` (configschema, contexts, TagValueProducer, TypeConversion, ConfigExpression/Parser, BooleanExpressionParser/Script, CEL stdlib on the `cel` crate, ConfiguredFeature, ConfiguredProfile, ConfiguredMapMain (`configure`, `create_planetiler`, `main`), bundled samples + JSON schemas, `ConfiguredMapTest`) | done (see notes below); deferred: `SchemaValidator` CLI |
| `feature_merge` (`FeatureMerge`: line merging via `LoopLineMerger`, nearby/overlapping polygon merging with buffer/union/unbuffer, multi-point/line/polygon merging, `remove_points_outside_buffer`, `group_by_attrs`, `extract_connected_components`) + full `FeatureMergeTest` | done (see notes below) |
| `reader::{simple_reader, source_feature_processor, shapefile_reader, geo_package_reader, natural_earth_reader}`, `reader::geojson::*`, `reader::parquet::*` + benchmark `benchmark_simple_readers` | done, including profile/`FeatureGroup` wiring (`source_feature_processor::process_files`, per-reader `process` entry points, `impl SourceFeature for ParquetFeature`); see notes for the orchestrator API |

### `geo` notes

- JTS `isValid` -> `geo::is_valid` (JTS semantics: no ring self-touch, holes may touch at single points as long as the
  interior stays connected). Prefer it over `geo::Validation`, which differs and is O(n^2).
- `buffer(0)` -> `polygon_overlay::buffer_zero` (positive winding), `GeometryFixer.fix` -> `polygon_overlay::geometry_fixer`
  (non-zero winding), `GeometryPrecisionReducer.reduce` -> `polygon_overlay::reduce_precision` (round to grid + re-node
  with `i_overlay`'s integer engine; never fails, so the JTS fallback paths in `snap_and_fix_polygon` are rarely hit
  and results can differ slightly from JTS snap-rounding). Overlay output: shells CW, holes CCW, OGC-valid.
- Empty geometries: `GeometryCollection(vec![])` (Java `EMPTY_GEOMETRY`), `Polygon` with empty exterior, empty
  `LineString`; geo has no empty point so `EMPTY_POINT` is an empty `MultiPoint`.
- CRS support (`geo::crs`) is limited to EPSG:4326/4269/3857 (+aliases), PROJ strings via `proj4rs`, and simple
  `GEOGCS`/web-mercator `PROJCS` WKT.
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
- JTS `BufferOp` with mitre joins -> `geo::Buffer` (`i_overlay` outline) with `LineJoin::Miter(2 * asin(1/5))`
  (JTS mitre limit 5). Negative buffers of multi-part input are applied to their union (like JTS).
- `buffer_union_unbuffer` succeeds on 3 of the protomaps538 inputs where JTS's unbuffer collapses
  (locationtech/jts#1183) and Java falls back to a plain union; the Rust result fills sub-pixel gaps (area up to ~3%
  larger than the input), so that test allows a 3% area increase instead of 1%. The `@Slow` Boston/Jakarta tests are
  `#[ignore]`d (run with `--release -- --ignored`, ~1s) and allow a 1% difference in the polygon count (Java's
  exact counts are within 0.5%).


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
  `file_utils::walk_path_with_pattern`) or a zip containing one shapefile; zip entries are streamed (not extracted).
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
  `(id, location)` pairs plus the remaining elements over a per-block channel to the single writer thread.
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
order. Any failure aborts every queue; `MLT` tile format and `UNKNOWN` compression return an error up front.
Encoding uses `VectorTile::encode_with_layer_stats` (same stats as `tile_size_stats::compute_tile_stats` without
building prost structs) and `util::gzip::gzip` (thread-local libdeflate, level 6; bytes differ from Java's zlib but
sizes are within ~0.5%). `TileWeights` implements `TileWeightSource`.

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
`brunnel` -> `utils::brunnel[_with_ford]` (`Option<&'static str>`). `VerifyMonaco` also hosts a local port of
`Verify.getNumFeatures`/`checkMinFeatureCount` (`TODO(port)`: move into `mbtiles::verify`).

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
- Not modelled: Java subtracts the size of `free_<name>_after_read` inputs from the write-phase disk estimate
  (resource amounts are unsigned); `LogUtil.setStage` prefixes; virtual-thread executor.
- Behaviour fixes made while comparing against Java on Monaco/Switzerland: `polygon_overlay::reduce_precision` now
  snaps each segment to every hot pixel it passes through (JTS `SnapRoundingNoder`, vertices only - intersection hot
  pixels are still left to the integer overlay), so sub-pixel slivers collapse like JTS; `FeatureGroup` writes a tile
  with an empty layer (like Java) when all of a layer's features are removed by `unscale/removePointsOutsideBuffer`;
  `test_utils::validate_geometry` uses JTS `isSimple` semantics like Java's `TestUtils`; custommap reads OSM tags via
  `WithTags::tags` (OSM features have empty `base().tags`); `extract_from_zip` handles relative tmp dirs.
- CLI (`crates/planetiler/src/main.rs`): `planetiler [task] [args]`, tasks as in `Main.java` (`openmaptiles` default
  with the maptiler URL overrides, `custom`, `shortbread`, `*.yml` first arg, `example-*`, `overture`, `osm-qa`,
  `verify-mbtiles`, `verify-monaco`, `stats`, `compare`). Logging via `tracing-subscriber` (`RUST_LOG`), messages
  written verbatim so progress-log ANSI colors render.
- Examples: `planetiler_examples::<module>::{main(args), run(..)}` for `toilets_overlay`, `toilets_overlay_low_level_api`,
  `bike_route_overlay`, `osm_qa_tiles`, `admin_borders_overlay`, `parks_overlay`, `tram_route_overlay`,
  `water_infrastructure_project`, `overture`.

**Comparison against Java** (`planetiler-dist-0.10.3-SNAPSHOT`, decoded tiles compared ignoring ring start/order):
- Toilets overlay (Monaco, `--process_threads=1` because the sort key is a processing-order counter): identical.
- `custom --schema=owg_simple.yml` (Monaco): identical apart from polygon ring start points/order.
- OpenMapTiles Monaco: same 23 tiles/5357 features; remaining differences: z13 merged buildings (`mergeNearbyPolygons`
  buffer/unbuffer output has ~50% more vertices than JTS mitre buffers, area within 0.5%), housenumber merged-point
  IDs (Java picks the first feature of a `HashMap` iteration), polygon ring start/order.
- OpenMapTiles Switzerland: see the final report of the orchestrator port (buffer-merge vertex counts for
  building/landuse/landcover/transportation, `transportation_name` line-merge differences at z12-14, data error counts
  `osm_boundary_missing_way`/`osm_multipolygon_missing_way` lower than Java).

## Follow-ups (tracked by the coordinator)

- Replace `planetiler-openmaptiles/src/feature_merge_shim.rs` with `planetiler_core::feature_merge` once FeatureMerge
  merges; drop the crate's `rstar`/`hashbrown`/`ahash` deps.
- Port JTS `Polygonizer` into core and use it for OpenMapTiles boundary country polygons (currently a line-merge +
  union stand-in that drops rings closing only through a node shared by 3+ ways).
- Move `Verify.checkMinFeatureCount`/`getNumFeatures` from `planetiler-openmaptiles/src/util/verify_monaco.rs` to
  `planetiler_core::mbtiles::verify`.
- Unify the two `VectorLayer` types (`archive::tile_archive_metadata` and `util::layer_attr_stats`).
- Re-enable the four `#[cfg(any())]` merge tests in `crates/planetiler-core/tests/` once FeatureMerge merges.
- `feature_merge::buffer_geometry`: mitre buffer output has ~50% more vertices than JTS (z13 OpenMapTiles buildings).
- `transportation_name` z12-14 differences vs Java on Switzerland (line merging / route attributes).
- Port custommap `SchemaValidator` CLI, `TopOsmTiles.main` (xz), `OpenMapTilesMapping` benchmark; in-memory stream
  archive readers for the csv/tsv/json/proto `testPlanetilerRunner` variants.
- MLT tile format (MapLibre Tile) encoder.
- Full benchmark re-run on a quiet machine (see `bench/RESULTS.md`).
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
(an `as_any` lifetime-erasure trick, see the SAFETY comment) and compare `kind()` to emulate Java's `instanceof`.

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
`OsmReader.canBeLine`. Tests: `tests/{configured_feature_test, expression_tests, schema_tests}.rs` (the latter has a
test-only port of `BaseSchemaValidator.validate` running the full shortbread spec, plus an ignored
`bench_shortbread` throughput check: ~0.9M features/s/thread in release).
- custommap: switch `tests/schema_tests.rs` to `planetiler_core::validator`; port the SchemaValidator CLI/watch.
- custommap CEL gaps vs cel-java (no type checker, `1 == 1.0` is true, Rust regex syntax in `replaceRegex`): consider
  closing these if real schemas hit them.
  `ParquetFeature` implements `SourceFeature` and has `into_reader_feature()`.
