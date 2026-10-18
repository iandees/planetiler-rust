# Results

Machine: Apple Silicon, 15 cores, 48 GB RAM. Java: OpenJDK 25.0.4.1. Rust: 1.99 `--release`.
Numbers are the best of the repeated runs each benchmark prints.

## Component benchmarks

| Benchmark | Java | Rust | Rust/Java |
|-----------|------|------|-----------|
| BenchmarkTileCoord (z0-z14 encode/decode) | 835M/s | 1.3B/s | 1.56x |
| BenchmarkVarInt (100M varints) | 184M/s | 239M/s | 1.30x |
| BenchmarkMbtilesWriter (1M tiles x10, compact, avg) | 2.93M/s | 3.74M/s | 1.28x |
| BenchmarkMbtilesRead (500k z14 800B tiles, avg) | 513k/s | 589k/s | 1.15x |
| BenchmarkPmtiles (60M dir entries encode+decode, best) | 45M/s (8.9s wall, 28s CPU total) | 105M/s (2.3s wall, 2.0s CPU total) | 2.3x |
| LongLongMapBench sparsearray/ram 100M, 8 readers: write / read | 201M/s / 100M/s | 281M/s / 223M/s | 1.4x / 2.2x |
| LongLongMapBench sparsearray/mmap: write / read | 124M/s / 102M/s | 266M/s / 251M/s | 2.1x / 2.5x |
| LongLongMapBench array/mmap: write / read | 412M/s / 140M/s | 422M/s / 441M/s | 1.0x / 3.2x |
| BenchmarkVectorTile (bench/java): encodeGeometry | 13.5M features/s | 11.6M features/s | **0.86x** |
| BenchmarkVectorTile: full 3-layer tile encode | 1634 tiles/s | 1476 tiles/s | **0.90x** |
| BenchmarkOsmRead pass1, Switzerland 549MB (15 threads) | ~1.2s wall, ~15s CPU | 1.0-1.3s wall, 13-16s CPU | ~1.0x |

Notes:
- TileCoord: the first straight port ran at 604M/s (0.72x Java). Replacing the zoom-start lookup table + linear scan
  with the closed form `(4^z - 1) / 3` / `leading_zeros`, and `/`,`%` by a power of two with shifts/masks, gave 1.3B/s.
  (The same change would likely help Java too; this compares the ports as they exist.)
- Run back-to-back under the same background load (load avg 6-16 from agent builds; rerun on idle machine later).
- PMTiles: Java's numbers include JIT warmup + GC across 3 rounds; the per-round rate is the fairer comparison.
- VectorTile is slower in Rust so far: candidate fixes are `Arc<Tags>` in `Feature` (avoid cloning tag maps),
  avoiding per-feature `Vec` allocations in the geometry encoder, and a faster hasher for key/value interning.
- OsmRead is at parity: Rust decodes via prost-generated structs (allocates per block); a hand-written zero-copy
  PBF decoder is the next step.

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
