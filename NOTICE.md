# Notices

planetiler-rust is a Rust port of [Planetiler](https://github.com/onthegomap/planetiler), licensed under the
Apache License, Version 2.0 (see [LICENSE](LICENSE)).

Planetiler is Copyright 2021 Michael Barry and Planetiler Contributors. This port is derived from Planetiler's source
code (commit `d88cf89`); see Planetiler's [NOTICE.md](https://github.com/onthegomap/planetiler/blob/main/NOTICE.md)
for the attributions of the code Planetiler itself adapts (imposm3, geojson-vt, java-vector-tile, osmosis, uppend,
graphhopper, hilbert_curves, OSM-binary, Bazel, ...). Those adapted algorithms are carried over into the
corresponding Rust files.

## Adapted code

In addition to Planetiler itself, this repository contains Rust ports or copies of:

- [JTS Topology Suite](https://github.com/locationtech/jts) (dual-licensed EPL 2.0 / Eclipse Distribution License 1.0; used here under the EDL): Douglas-Peucker
  simplifier, buffer/offset-curve builder (`geo/buffer/`), CascadedPolygonUnion, Polygonizer, IsValidOp, InteriorPoint,
  Centroid, MaximumInscribedCircle, LineMerger, RobustLineIntersector, double-double orientation predicates and
  normalization (`crates/planetiler-core/src/geo/`).
- [MapLibre Tile (MLT)](https://github.com/maplibre/maplibre-tile-spec) Java encoder `org.maplibre:mlt` 0.0.10
  (Apache License 2.0) (`crates/planetiler-core/src/mlt/`), including its use of:
  - [JavaFastPFOR](https://github.com/lemire/JavaFastPFOR) 0.3.10 (Apache License 2.0)
  - the pure-Java FSST encoder from maplibre-tile-spec (Apache License 2.0), after
    [FSST](https://github.com/cwida/fsst) (MIT License)
  - [earcut4j](https://github.com/earcut4j/earcut4j) 3.0.0 (ISC License), after
    [earcut](https://github.com/mapbox/earcut) (ISC License)
- [i_float](https://github.com/iShape-Rust/iFloat) 5.0.0 (MIT License), vendored with a faster exact division in
  `vendor/i_float/` (see `vendor/i_float/LICENSE`).
- [PROJ](https://proj.org) (MIT License): the formulas of the map projections in
  `crates/planetiler-core/src/geo/crs/projections.rs` (Cassini-Soldner, Hotine oblique mercator, polyconic,
  sinusoidal, Robinson, equidistant conic, gnomonic, orthographic, Equal Earth, New Zealand Map Grid, Lambert
  azimuthal equal area) are ports of PROJ's implementations.
- Protocol buffer definitions `osmformat.proto` / `fileformat.proto` from
  [OSM-binary](https://github.com/openstreetmap/OSM-binary) (MIT License) and the Mapbox
  [vector tile spec](https://github.com/mapbox/vector-tile-spec) `vector_tile.proto` (CC0/BSD).

## planetiler-openmaptiles

`crates/planetiler-openmaptiles` is a port of
[planetiler-openmaptiles](https://github.com/openmaptiles/planetiler-openmaptiles) and vendors the
[OpenMapTiles](https://github.com/openmaptiles/openmaptiles) schema (commit `e0fb503`). The code is licensed under the
BSD 3-Clause License (see [crates/planetiler-openmaptiles/LICENSE.md](crates/planetiler-openmaptiles/LICENSE.md)).

The cartography and visual design features of the map tile schema are licensed under
[CC-BY 4.0](http://creativecommons.org/licenses/by/4.0/). Products or services using maps derived from the
OpenMapTiles schema need to visibly credit "OpenMapTiles.org" or reference "OpenMapTiles" with a link to
http://openmaptiles.org/.

## Data

`crates/planetiler-core/src/geo/crs/epsg.tsv.zst` contains coordinate reference system definitions derived from the
[EPSG Geodetic Parameter Dataset](https://epsg.org) (version 11.031, as distributed in
[GeoTools](https://geotools.org) `gt-epsg-hsql` 35.1), © International Association of Oil & Gas Producers (IOGP),
used under the [EPSG terms of use](https://epsg.org/terms-of-use.html). The data has been converted to PROJ strings
(with the datum shifts GeoTools selects) and is not the official EPSG dataset; see https://epsg.org for that.

Test resources include small extracts of OpenStreetMap data (© OpenStreetMap contributors,
[ODbL](https://www.openstreetmap.org/copyright)), Natural Earth (public domain), OSM water polygons (ODbL) and Overture
Maps samples ([various licenses](https://docs.overturemaps.org/attribution)), copied from Planetiler's test resources.
