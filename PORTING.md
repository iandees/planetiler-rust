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
| `vector_tile` (VectorTile, VectorGeometry, VectorGeometryMerger, Feature, encode/decode), `util::{tile_size_stats, layer_attr_stats, tileset_summary_statistics}` | done (MLT encoding/stats, `TileSizeStats.main` CLI, TileWeights integration deferred: see `TODO(port)`) |
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
  `ReaderFeature { geometry (lat/lon), tags, source_layer, id }` (the arguments of Java's `SimpleFeature.create`).
  Readers may decode in parallel internally but always call the sink from the calling thread.
  **TODO(port) for whoever ports `SimpleFeature`/`Profile`/`FeatureGroup`:** add `ReaderFeature -> SimpleFeature`
  (`SimpleFeature.create(geometry, tags, sourceName, sourceLayer, id)`), then build Java's
  `SourceFeatureProcessor.processFiles(name, paths, readerFactory, writer, config, profile, stats)` on top of
  `source_feature_processor::process_files(name, paths, reader_factory, config, stats, processor_factory)` (the
  per-thread `processor_factory` closure should convert to `SimpleFeature`, run `profile.process_feature`, render
  into `writer.writer_for_thread()`, and `profile.finish(..)` must be called afterwards), and the static
  `ShapefileReader.processWithProjection`, `GeoPackageReader.process`, `NaturalEarthReader.process`,
  `GeoJsonReader.process` entry points. Same for `ParquetReader::process(paths, config, cares_about,
  processor_factory)` (pass `profile.cares_about`, wrap `ParquetFeature` as a `SourceFeature`).
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

