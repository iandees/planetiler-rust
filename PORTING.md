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
| `reader::osm::{pbf_decoder, pbf_field_decoder, osm_element, osm_header, osm_input_file, osm_block_source, poly_file_reader}` (raw/zlib/lz4/zstd, eager or mmap lazy reads) | done |
| `expression::{expression, multi_expression, data_type, simplifiable}` (input trait `ExpressionInput`) | done |
| `planetiler-benchmarks` `benchmark_osm_read` binary (emulates `OsmReader.pass1` until it is ported) | done |
| `collection` (append stores, `LongLongMap` + impls, `LongLongMultimap`, heaps, `LongMerger`, `IntRangeSet`, `SortableFeature`, `FeatureSort`, `ExternalMergeSort`) + `LongLongMapBench` (`planetiler-benchmarks` bin `long_long_map_bench`) | done (`feature_group` is wave 2) |
| `config` (`Arguments`, `PlanetilerConfig::from`, `Bounds`) | done (`--polygon` shape and Bounds covered-tiles from shape: TODO) |
| `stats` (`Stats`, `InMemoryStats`, `PrometheusStats`, `DefaultStats`, `ProgressLoggers`, `ProcessInfo`, `Timers`, `Timer`, `ProcessTime`, `Counter`) | done (no GC/heap stats; per-thread CPU via thread registry) |
| `worker` (`Worker`, `WorkerPipeline`, `WorkQueue`, `WeightedHandoffQueue`, `Distributor`) | done (`processAndWrite` belongs with `FeatureGroup`) |
| `util` (format, ansi_colors, parse, imposm3_parsers, var_int, hashing, sort_key, merging_range_map, sliding_window, zoom_function, cache_by_zoom, bin_pack, common_string_encoder, byte_buffer_util, gzip, yaml, build_info, madvise, memory_estimator, file_utils, file_watcher, resource_usage, tile_weights, downloader, aws_osm, geofabrik, overture_stac, top_osm_tiles) | done |
| benchmarks: `benchmark_var_int`, `benchmark_tile_coord` | done |
| Archives (`archive::{tile, tile_encoding_result, readable_tile_archive, writeable_tile_archive, tile_archive_config, tile_archive_metadata(_de_ser), tile_archives}`, `mbtiles`, `pmtiles`, `files`, `stream`, `util::compare_archives`, `geo::tile_order`) | done; deferred: `TileArchiveMetadata::from_profile` (needs `Profile`), `ArchiveOptions` stands in for the unported `Arguments` API, `VectorLayer`/`FieldType` live in `archive::tile_archive_metadata` until `util::layer_attr_stats` is ported, geometry-level checks in `mbtiles::{verify, compare}` / `CompareArchives` wait on `vector_tile` decoding |
| Benchmarks `benchmark_mbtiles_writer`, `benchmark_mbtiles_read`, `benchmark_pmtiles` (`crates/planetiler-benchmarks/src/bin`) | done |
| `geo` (geo_utils, simplifiers, smoothers, line_splitter, geometry_pipeline/transformer, point/polygon index, unit, with_geometry, tile_order, simplify_method) + JTS ports (`is_valid`, `polygon_overlay` fix/precision-reduce/boolean ops, `interior_point`/centroid, `maximum_inscribed_circle`, `normalize`, `precision_model`, `wkt`, `crs`) + `util::loop_line_merger` + `geo::test_utils` | done (see notes below) |
| `reader::osm::{osm_multipolygon, osm_way_splitter, osm_node_bounds_provider}` | done (`NodeLocationProvider` trait lives in `osm_multipolygon` until `OsmReader` is ported; bounds provider does not use `OsmPhaser` yet) |
| `util::{translations, language_utils, thread_local_transliterator, wikidata}` | done (`Wikidata` takes a `Fn(&OsmElement) -> bool` filter instead of `Profile`: see `TODO(port)`) |
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
