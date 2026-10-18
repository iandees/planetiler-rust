# planetiler-rust

Generate [vector tiles](https://github.com/mapbox/vector-tile-spec) from
[OpenStreetMap](https://www.openstreetmap.org/) and other geographic data, from a city up to the whole planet, on a
single machine. Output is an MBTiles or PMTiles file (or a directory of tiles) that you can serve or open directly in
[MapLibre](https://maplibre.org/).

This is a Rust port of [Planetiler](https://github.com/onthegomap/planetiler). It takes the same commands and options
and writes the same tiles, and runs about 3.5x faster with 58% less memory
([benchmarks](bench/RESULTS.md)). Details of the port are in [PORTING.md](PORTING.md).

## Quick start

You need a recent stable Rust toolchain.

```sh
cargo build --release

# download OpenStreetMap data for Monaco plus the ocean, lake and Natural Earth sources,
# then build an OpenMapTiles basemap
target/release/planetiler --download --area=monaco --output=data/monaco.pmtiles
```

Drag `data/monaco.pmtiles` onto [pmtiles.io](https://pmtiles.io) to look at it. For a styled map, point a MapLibre
style that uses the [OpenMapTiles schema](https://openmaptiles.org/schema/) (for example
[OSM Bright](https://github.com/openmaptiles/osm-bright-gl-style)) at the archive. MBTiles output can be served
with [tileserver-gl](https://github.com/maptiler/tileserver-gl) or [martin](https://github.com/maplibre/martin).

The first run downloads about 1.3 GB of global sources (oceans, Natural Earth, lake centerlines) into
`data/sources/`. Later runs reuse them.

## Building a basemap

The default task builds an [OpenMapTiles](https://openmaptiles.org/) basemap.

```sh
# any Geofabrik extract by name ("switzerland", "new-york", "planet", ...)
target/release/planetiler --download --area=switzerland --output=data/switzerland.mbtiles

# a .osm.pbf you already have
target/release/planetiler --download --osm-path=path/to/region.osm.pbf --output=data/region.pmtiles

# only part of the extract, a smaller zoom range, a few languages
target/release/planetiler --download --area=germany --bounds=13.0,52.3,13.8,52.7 \
  --maxzoom=12 --languages=en,de --output=data/berlin.pmtiles
```

Layer options: `--only-layers=water,landcover,...` and `--exclude-layers=...` choose layers, and
`--transportation-z13-paths`, `--building-merge-z13`, `--boundary-country-names` and other layer switches tune the
output (see `--help`).

Maps built with OpenMapTiles must credit "© OpenMapTiles © OpenStreetMap contributors".

## Making your own map

Describe the layers you want in a YAML schema. This one makes a layer of manhole covers from OpenStreetMap:

```yaml
schema_name: Manhole covers
attribution: <a href="https://www.openstreetmap.org/copyright">&copy; OpenStreetMap contributors</a>
sources:
  osm:
    type: osm
    url: geofabrik:rhode-island
layers:
- id: manhole
  features:
  - source: osm
    geometry: point
    min_zoom: 14
    include_when:
      man_made: manhole
    attributes:
    - key: manhole
    - key: operator
```

```sh
target/release/planetiler manholes.yml --download        # same as: custom --schema=manholes.yml
target/release/planetiler shortbread --download --area=monaco
```

Schemas can filter and transform features with [CEL](https://cel.dev) expressions, read shapefiles, GeoPackages
and GeoJSON as well as OSM, and merge lines and polygons after tiling. The full reference is upstream
Planetiler's [custommap docs](https://github.com/onthegomap/planetiler/tree/main/planetiler-custommap). Bundled
samples work by name: `shortbread.yml`, `owg_simple.yml`, `power.yml`, `railways.yml`, `highway_areas.yml`,
`manholes.yml`, `natural_earth.yml`.

Put example inputs and expected outputs in the schema (or a separate spec file) and check them while you edit:

```sh
target/release/planetiler verify manholes.yml --watch
```

## Writing a profile in Rust

For full control, implement the `Profile` trait and run it with the `Planetiler` builder:

```rust
use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::source_feature::SourceFeature;

struct Toilets;

impl Profile for Toilets {
    fn process_feature(&self, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if feature.is_point() && feature.has_tag_str("amenity", "toilets") {
            features.point("toilets").set_zoom_range(0, 14);
        }
    }

    fn name(&self) -> String {
        "Toilets".into()
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    Planetiler::create(Arguments::from_env_or_args(&args))?
        .set_profile(Toilets)
        .add_osm_source_with_url("osm", "data/sources/monaco.osm.pbf", Some("geofabrik:monaco"))?
        .overwrite_output("data/toilets.mbtiles")?
        .run()
}
```

This compiles as `cargo run --release -p planetiler-examples --example readme_profile -- --download`.
`crates/planetiler-examples` has complete examples (toilets, bike routes, parks, OSM QA tiles, Overture buildings)
that you can also run from the CLI, e.g. `planetiler example-toilets --download`.

## Inputs

| Source | How to use it |
|---|---|
| OpenStreetMap `.osm.pbf` | `--area=<geofabrik name>` / `--osm-url=...` with `--download`, or `--osm-path=file.osm.pbf` |
| Shapefile (plain or zipped) | `type: shapefile` in YAML, `add_shapefile_source` in Rust |
| GeoPackage | `type: geopackage` / `add_geopackage_source` |
| GeoJSON (also newline-delimited) | `type: geojson` / `add_geojson_source` |
| GeoParquet, including [Overture Maps](https://overturemaps.org/) | `add_parquet_source` in Rust; `planetiler overture --base=<dir>` for Overture buildings |
| Natural Earth (SQLite) | `add_natural_earth_source` in Rust |

Shapefiles and GeoPackages can be in any projection: EPSG codes, `.prj` files (ESRI and OGC WKT), WKT2 and PROJ
strings are supported. Grid-based datum shifts (NTv2, NADCON) are not.

## Output

The output format comes from the `--output` file name:

| Output | Example |
|---|---|
| MBTiles (SQLite) | `--output=out.mbtiles` |
| PMTiles (single file, serve from static hosting) | `--output=out.pmtiles` |
| Directory of `{z}/{x}/{y}.pbf` files | `--output=out/` or `--output=files:///path?format=...` |
| CSV / TSV / JSON / protobuf streams | `--output=out.csv`, `.tsv`, `.json`, `.proto` |

Tiles are [Mapbox Vector Tiles](https://github.com/mapbox/vector-tile-spec) by default. Add `--tile-format=mlt` for
[MapLibre Tiles](https://github.com/maplibre/maplibre-tile-spec), and `--mlt-advanced=true` for FSST and FastPFOR
compression (smaller tiles, needs an MLT-capable client).

## Common options

Options are `--key=value` (or `--key value`). `-` and `_` are interchangeable, and every option can also be set with a
`PLANETILER_KEY=value` environment variable or in a `--config=file.properties` file. `planetiler --help` lists them
all with their defaults.

| Option | Default | |
|---|---|---|
| `--download` | `false` | Download missing sources |
| `--area` | `monaco` | Geofabrik extract to download (`planet` for the whole world) |
| `--osm-path` | `data/sources/<area>.osm.pbf` | Local OSM input |
| `--output` | `data/output.mbtiles` | Output archive |
| `--force` | `false` | Overwrite the output and ignore disk/memory warnings |
| `--bounds` | whole input | `minlon,minlat,maxlon,maxlat` or `world` |
| `--polygon` | | `.poly` file to limit output to |
| `--minzoom` / `--maxzoom` | `0` / `14` | Zoom range (max 16) |
| `--threads` | all cores | Worker threads |
| `--tmpdir` | `data/tmp` | Temporary files; put it on a fast SSD with free space |
| `--storage` | `mmap` | Temp storage: `mmap` (on disk, memory-mapped) or `ram` |
| `--nodemap-type` | `sparsearray` | Node location map: `sparsearray`, `array` (best for the planet), `sortedtable` |
| `--languages` | profile default | Name languages to include, e.g. `en,de,fr` |
| `--tile-format` | `mvt` | `mvt` or `mlt` |
| `--tile-compression` | `gzip` | `gzip` or `none` |
| `--skip-filled-tiles` | `false` | Leave out tiles that are only ocean or land fill |
| `--free-osm-after-read` | `false` | Delete inputs after reading to save disk (also `--free-<source>-after-read`) |
| `--fetch-wikidata` | `false` | Fetch Wikidata name translations for more languages |

Set `RUST_LOG=debug` for more logging.

## How big a machine?

Measured with OpenMapTiles on a 15-core Apple M5 Pro with 48 GB of RAM:

| Input | `.osm.pbf` size | Time | Peak memory | Output |
|---|---|---|---|---|
| Monaco | 0.4 MB | 1.6 s | 0.8 GB | 0.3 MB |
| Switzerland | 549 MB | 11 s | 4.0 GB | 362 MB |

Run time and temporary disk space grow roughly with the size of the input. For large extracts or the planet, leave
`--storage=mmap` (the default) so node locations and temporary features live in memory-mapped files on disk, and give
`--tmpdir` a fast SSD with several times the input size free. Planetiler checks memory and disk before it starts and
stops with an estimate if it thinks they're too small (`--force` overrides this). Upstream Planetiler's
[planet guide](https://github.com/onthegomap/planetiler/blob/main/PLANET.md) describes planet-scale setups.

## Other commands

| Command | What it does |
|---|---|
| `openmaptiles` (default) | Build an OpenMapTiles basemap |
| `custom --schema=<file>`, `<file>.yml`, `shortbread` | Build a map from a YAML schema |
| `verify <schema>.yml [--watch]` | Check a schema against its examples |
| `example-toilets`, `example-bikeroutes`, `example-qa`, `overture`, ... | Example profiles |
| `compare <a> <b>` | Compare two archives feature by feature (`--ignore-ids=true`, `--strict=false` to ignore metadata) |
| `stats --input=<archive>` | Tile and layer size statistics (MVT archives) |
| `verify-mbtiles <file>` | Check an MBTiles file's metadata and tiles |
| `top-osm-tiles` | Build a tile-weights file from openstreetmap.org tile logs |

## Building from source

```sh
cargo build --release            # target/release/planetiler
cargo test --workspace           # tests
```

`mimalloc` is the default memory allocator; build with `--no-default-features` to use the system allocator.
Benchmarks against Java are in `crates/planetiler-benchmarks` and `bench/` (see [bench/README.md](bench/README.md)).

## License

Apache License 2.0 (see [LICENSE](LICENSE)). `crates/planetiler-openmaptiles` is BSD 3-Clause. See
[NOTICE.md](NOTICE.md) for attributions. OpenStreetMap data is © OpenStreetMap contributors (ODbL), and maps built
with the OpenMapTiles schema must credit OpenMapTiles.
