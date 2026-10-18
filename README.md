# Planetiler (Rust port)

A Rust port of [Planetiler](https://github.com/onthegomap/planetiler), the tool that generates
[vector tiles](https://github.com/mapbox/vector-tile-spec) from geographic data sources like
[OpenStreetMap](https://www.openstreetmap.org/). It covers the same pieces as the Java project:

- the **OpenMapTiles** profile (all 16 layers, generated from the vendored OpenMapTiles schema),
- **custom maps** defined in YAML (the `planetiler-custommap` schema, including the bundled `shortbread.yml` and other
  samples) and the schema validator,
- the **example profiles** (toilets, bike routes, OSM QA tiles, Overture, ...),
- the core library: OSM `.osm.pbf`, shapefile, GeoPackage, Natural Earth, GeoJSON and (Geo)Parquet readers, the
  feature pipeline, external merge sort, and MBTiles / PMTiles / files / CSV / TSV / JSON / protobuf output archives,
- the `planetiler` CLI with the same task names and `--key=value` arguments as `planetiler.jar`.

The code mirrors the Java layout file by file (see [PORTING.md](PORTING.md)), so the upstream docs apply.

## Status

The port is feature-complete for OSM-based OpenMapTiles, custom YAML maps and the examples. Its output matches Java
closely: Monaco output is identical, and on Switzerland both produce the same 23,922 tiles with the same per-layer
feature counts in all 16 layers. The remaining differences are sub-pixel vertex differences in a few hundred merged
polygons and 10 merged lines (see [docs/switzerland-parity.md](docs/switzerland-parity.md)).

Performance, OpenMapTiles for Switzerland (549 MB `.osm.pbf`) on a 15-core Apple M5 Pro, from
[bench/RESULTS.md](bench/RESULTS.md):

| | Java | Rust | |
|---|---|---|---|
| Wall time | 37.4s | 10.7s | **3.5x faster** |
| CPU time | 313s | 121s | **2.6x less CPU** (61% less) |
| Peak RSS | 9.6 GB | 4.0 GB | **58% less RAM** |

Rust is faster in every stage of the pipeline (OSM pass 1 and 2, sort, archive, simple sources). On Monaco it takes
1.6s vs 14.1s wall, 11.0s vs 20.2s CPU and 0.83 vs 1.02 GB peak RSS. See [bench/RESULTS.md](bench/RESULTS.md) for
the methodology and the component benchmarks.

## Building

You need a recent stable Rust toolchain (edition 2024; CI uses the latest stable).

```sh
cargo build --release
# the binary is target/release/planetiler
```

`mimalloc` is the default global allocator (`--no-default-features` turns it off). With the macOS system allocator,
OSM decoding was up to 2.5x slower in some runs because of false sharing; see bench/RESULTS.md.

Run the tests with `cargo test --workspace`.

## Running

Arguments use the same names as the Java version (`--key=value`, `--key value`, `key=value`, `PLANETILER_KEY=value`
environment variables, or a `--config=file.properties` file). `-` and `_` are interchangeable.

### OpenMapTiles (the default task)

```sh
# download Monaco and the other sources into data/sources, then write data/output.mbtiles
target/release/planetiler --download --area=monaco

# a local extract, PMTiles output
target/release/planetiler --osm-path=data/sources/switzerland.osm.pbf --output=data/switzerland.pmtiles --download
```

`--area` takes a Geofabrik extract name (or `planet`). The common options all work: `--output`, `--bounds`,
`--minzoom`/`--maxzoom`, `--threads`, `--force`, `--nodemap-type`/`--nodemap-storage`, `--languages`, and so on.

### Custom YAML maps

```sh
target/release/planetiler custom --schema=path/to/schema.yml --download
target/release/planetiler path/to/schema.yml --download         # shortcut for the above
target/release/planetiler shortbread --area=monaco --download   # bundled shortbread.yml
target/release/planetiler custom --schema=owg_simple.yml          # bundled samples work by name
```

Check a schema against its examples (embedded under `examples:` or in a separate spec file):

```sh
target/release/planetiler verify path/to/schema.yml          # add --watch to re-run on every change
```

### Examples

```sh
target/release/planetiler example-toilets --download             # data/toilets.mbtiles
target/release/planetiler example-bikeroutes --download
target/release/planetiler example-toilets-lowlevel --download
target/release/planetiler example-qa --download                  # OSM QA tiles
target/release/planetiler example-overture --base=data/overture  # Overture Maps buildings (GeoParquet)
```

The example profiles live in `crates/planetiler-examples/src` and show how to write a profile in Rust by implementing
the `Profile` trait and running it with `planetiler_core::planetiler::Planetiler`.

### CLI tasks

| Task | What it does |
|---|---|
| `openmaptiles` / `generate-openmaptiles` (default) | Build an OpenMapTiles basemap |
| `custom` / `generate-custom`, `<schema>.yml` | Build a map from a YAML schema |
| `shortbread` / `generate-shortbread` | Build a map with the bundled Shortbread schema |
| `verify` / `verify-custom` / `verify-schema` | Check a YAML schema against its examples (`--watch`) |
| `example-toilets`, `example-toilets-lowlevel`, `example-bikeroutes`, `example-qa` / `osm-qa`, `example-overture` / `overture` | Example profiles |
| `verify-mbtiles <file>` | Basic checks on an MBTiles file (metadata, tiles decode, polygons are valid) |
| `verify-monaco <file>` | `verify-mbtiles` plus feature counts expected in an OpenMapTiles Monaco build |
| `stats --input=<archive>` | Per-tile/per-layer size statistics (`TileSizeStats`) |
| `compare [--verbose] <archive1> <archive2>` | Compare two archives tile by tile |

Logging uses `RUST_LOG` (for example `RUST_LOG=debug`).

`compare` decodes tiles whose bytes differ and reports a diff only when the decoded contents differ: layers are
paired by name, feature tags are compared as decoded key/value pairs, and geometries are compared by their decoded
value. Tiles that differ only in encoding (key/value table order, layer order) are counted separately as
"equivalent". This makes it useful for comparing Java and Rust output.

Benchmarks are separate binaries in `crates/planetiler-benchmarks` (`cargo run --release -p planetiler-benchmarks
--bin <name>`); `bench/run_all.sh` runs the Java-vs-Rust suite.

## Differences from Java

- **Profiles are Rust code.** Write a profile by implementing `planetiler_core::profile::Profile`; there's no JVM
  plugin loading. YAML schemas work the same way as in Java.
- **JVM options don't apply.** There's no `-Xmx`. Memory checks use the machine's physical memory, and the same
  `--nodemap-storage`/`--nodemap-type` options choose between RAM and memory-mapped storage.
- **Profile errors.** Rust has no `Error`/`Exception` split, so every panic in a profile is caught and logged as a
  data error (Java lets `Error`s abort the run).
- **Custom map expressions** use the [`cel`](https://crates.io/crates/cel) crate. Unlike cel-java there is no type
  checker (type errors show up at runtime), `1 == 1.0` is true, and `replaceRegex` uses Rust `regex` syntax (no
  look-around or backreferences).
- **Parquet/struct values.** Temporal and UUID columns become their Java `toString()` strings.
- **`compare`** only reports real content differences (see above). Java also counts tiles whose bytes differ.
- **Logging.** There's no `[stage]` prefix on log lines, and data-error debug logs print WKB as hex rather than base64.

## Known gaps

From [PORTING.md](PORTING.md) and the `TODO(port)` triage in
[docs/review-unsafe-and-todos.md](docs/review-unsafe-and-todos.md):

- **MLT (MapLibre Tile) output** is not supported (`--tile-format=mlt` is an error). MVT is the default and fully
  supported.
- **OpenMapTiles country boundaries.** `adm0_l`/`adm0_r` are computed from merged boundary lines rather than a JTS
  `Polygonizer`, so rings that only close through a node shared by 3 or more ways are dropped.
- **Snap rounding.** `reduce_precision` adds hot pixels for vertices but not for segment intersections, and lines are
  rounded point by point, so a small number of output geometries differ from JTS.
- **Merged polygon buffering** uses a different offset-curve implementation from JTS, so merged buildings and landuse
  can differ by sub-pixel vertices.
- **Coordinate reference systems.** Only EPSG:4326, 4269 and 3857, PROJ strings and simple WKT are supported. Inputs
  in other projections fail where Java (GeoTools) succeeds.
- **CLI tasks not ported:** `top-osm-tiles` (needs xz decompression) and `benchmark-mapping`.
  `benchmark-longlongmap` points you at the benchmarks crate instead.
- **Parquet load balancing.** Parquet row groups aren't redistributed between threads (Java's `Distributor`), which
  can leave threads idle on inputs with uneven row-group sizes.

## License

Apache-2.0, like upstream Planetiler. Map data from OpenStreetMap is © OpenStreetMap contributors.
