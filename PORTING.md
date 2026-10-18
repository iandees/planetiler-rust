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
| `reader::osm::{osm_multipolygon, osm_way_splitter, osm_node_bounds_provider}` | done (`NodeLocationProvider` trait lives in `osm_multipolygon` until `OsmReader` is ported; bounds provider does not use `OsmPhaser` yet) |
| `util::{translations, language_utils, thread_local_transliterator, wikidata}` | done (`Wikidata` takes a `Fn(&OsmElement) -> bool` filter instead of `Profile`: see `TODO(port)`) |
| Feature pipeline spine: `profile` (`Profile`, `NullProfile`), `feature_processor`, `forwarding_profile`, `feature_collector`, `reader::{source_feature, simple_feature}`, `reader::osm::osm_relation_info` (+ `RelationMember`), `render::{feature_renderer, tiled_geometry, geometry_coordinate_sequences, rendered_feature}`, `collection::feature_group` | done (see notes below) |
| `planetiler-openmaptiles`: `Generate` (generator + vendored schema), generated `OpenMapTilesSchema`/`Tables`, `OpenMapTilesProfile`, `OpenMapTilesMain` (setup only), all 16 layers, `OmtLanguageUtils`, `Utils`, `VerifyMonaco`, all unit tests (249) | done (see notes below; deferred: `OpenMapTilesTest` end-to-end + runner call wait on `Planetiler`; uses a FeatureMerge copy until core lands it) |

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
`run` has a `TODO(port)` for the `Planetiler` runner call. Sources: `lake_centerlines` (shapefile, EPSG:3857,
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
`OpenMapTilesTest` (end-to-end Monaco build) is deferred until the `Planetiler` runner is ported.
