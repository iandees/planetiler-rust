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
