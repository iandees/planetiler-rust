# Results

Machine: Apple Silicon, 15 cores, 48 GB RAM. Java: OpenJDK 25.0.4.1. Rust: 1.99 `--release`.
Numbers are the best of the repeated runs each benchmark prints.

## Component benchmarks

| Benchmark | Java | Rust | Rust/Java |
|-----------|------|------|-----------|
| BenchmarkTileCoord (z0-z14 encode/decode) | 835M/s | 1.3B/s | 1.56x |
| BenchmarkVarInt (100M varints) | 184M/s | 239M/s | 1.30x |

Notes:
- TileCoord: the first straight port ran at 604M/s (0.72x Java). Replacing the zoom-start lookup table + linear scan
  with the closed form `(4^z - 1) / 3` / `leading_zeros`, and `/`,`%` by a power of two with shifts/masks, gave 1.3B/s.
  (The same change would likely help Java too; this compares the ports as they exist.)
- Run back-to-back under the same background load (~16).

## End-to-end

| Run | Java wall | Java CPU | Java peak RSS | Rust wall | Rust CPU | Rust peak RSS |
|-----|-----------|----------|---------------|-----------|----------|---------------|
| OpenMapTiles, Monaco, sources cached (`-Xmx8g`) | 14.4s | 21.2s | 1.10 GB | | | |

Monaco stage breakdown (Java): water_polygons 5s, natural_earth 7s, osm_pass1 0.2s, osm_pass2 0.3s, sort 0s,
archive 0.6s. Monaco is dominated by reading the global ocean/Natural Earth sources, so it's a correctness check more
than a speed test; Switzerland (549 MB) is the main end-to-end benchmark.

Command (run from `../planetiler-bench`, with background builds idle):

```sh
/usr/bin/time -l java -Xmx8g -jar planetiler-dist-*-with-deps.jar --osm-path=monaco-latest.osm.pbf --output=java-monaco.mbtiles --force
```
