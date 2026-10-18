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
| `planetiler-custommap` (configschema, contexts, TagValueProducer, TypeConversion, ConfigExpression/Parser, BooleanExpressionParser/Script, CEL stdlib on the `cel` crate, ConfiguredFeature, ConfiguredProfile, ConfiguredMapMain `configure`, bundled samples + JSON schemas) | done (see notes below); deferred: `post_process_layer_features` merging (needs `FeatureMerge`), the planetiler run in `ConfiguredMapMain` (needs `Planetiler`), `SchemaValidator` CLI (needs `planetiler_core::validator`), `ConfiguredMapTest`/`VerifyMonaco` (full pipeline) |

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

### custommap notes (`crates/planetiler-custommap`)

**Loading.** `SchemaConfig::load(path)` / `load_str(yaml)` go through `util::yaml` (anchors, aliases, `<<` merge keys)
then serde with `deny_unknown_fields` (Jackson's default). Java `Object` fields are `Option<Value>` (mapping keys become
strings like Jackson; YAML `null` == missing). `ConfiguredMapMain` -> `configured_map_main::{configure(&Arguments) ->
ConfiguredMap { root, profile: Arc<ConfiguredProfile>, sources: Vec<ConfiguredSource> }, load_schema, main}`;
`main` stops with a `TODO(port)` where it would build the `Planetiler` run. Samples are bundled with `include_str!`
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
`post_process_layer_features` is a `TODO(port)` until `FeatureMerge` exists. `configured_profile::can_be_line` ports
`OsmReader.canBeLine`. Tests: `tests/{configured_feature_test, expression_tests, schema_tests}.rs` (the latter has a
test-only port of `BaseSchemaValidator.validate` running the full shortbread spec, plus an ignored
`bench_shortbread` throughput check: ~0.9M features/s/thread in release).
