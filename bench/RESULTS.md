# Results

Machine: Apple Silicon, 15 cores, 48 GB RAM. Java: OpenJDK 25.0.4.1. Rust: 1.99 `--release`.
Numbers are the best of the repeated runs each benchmark prints.

**Provisional:** everything below was measured while other agents were compiling (load avg 5-50). It will be re-measured
sequentially on a quiet machine by a dedicated benchmark pass; treat ratios as rough until then.

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
| BenchmarkVectorTile (bench/java): encodeGeometry | 12.4-14.7M features/s | 31-32M features/s | 2.2-2.5x |
| BenchmarkVectorTile: full 3-layer tile encode | 1590-1713 tiles/s | 3750-3920 tiles/s | 2.3x |
| BenchmarkOsmRead pass1, Switzerland 549MB (15 threads) | 0.9-1.0s wall, 12-13s CPU | 0.57-0.60s wall, 7.5-7.9s CPU | 1.6x wall / 1.6x CPU |
| BenchmarkLineSplitter ms/run (reused 3 sizes; !reused; cacheable) — lower is better | 36/12/6; 77/45/39; 40/12/6 | 39/10/3; 66/24/14; 43/11/4 | ~1-2.8x |
| BenchmarkLineMerge loop(0) rates (/s, 4 workloads) | 11k/108k/282k/404k | 7.6k/72k/161k/224k | **0.55-0.7x** |
| BenchmarkLineMerge JTS LineMerger (2 timings, lower is better) | 0.30s/0.34s | 0.18s/0.21s | 1.6x |
| BenchmarkSimplify DP(1) (circles of 2/10/50/100/10k pts) | 39M/14M/4.5M/2.3M/25k | 39M/14M/3.4M/1.8M/21k | **0.75-1.0x** |
| BenchmarkSimplify VW(0) (Planetiler's VW) | 13M/4.4M/1M/548k/3.2k | 9.8M/6.7M/2M/1.1M/5.8k | 0.75x small, ~2x large |

Notes:
- TileCoord: the first straight port ran at 604M/s (0.72x Java). Replacing the zoom-start lookup table + linear scan
  with the closed form `(4^z - 1) / 3` / `leading_zeros`, and `/`,`%` by a power of two with shifts/masks, gave 1.3B/s.
  (The same change would likely help Java too; this compares the ports as they exist.)
- Run back-to-back under the same background load (load avg 6-16 from agent builds; rerun on idle machine later).
- PMTiles: Java's numbers include JIT warmup + GC across 3 rounds; the per-round rate is the fairer comparison.
- VectorTile: the straight port ran at 11.6-12.0M features/s and 1476-1555 tiles/s (0.86-0.9x Java). Now
  `VectorTile::encode` writes protobuf bytes directly into an exactly-sized buffer (sizes computed in a first pass)
  instead of building prost structs, per-feature tag index pairs share one `Vec` per layer, key/value interning uses
  foldhash instead of SipHash, and geometry encoding reuses a thread-local command buffer (one allocation per
  geometry). Both benchmarks print an FNV-1a hash of the encoded tile; Rust and Java match (`8512da4e8a2d855c`).
  `Feature.tags` is still an owned `Tags` (the benchmark doesn't clone tags in the timed loop, so `Arc<Tags>` was not
  needed here).
- OsmRead: the prost-based decoder ran at 0.95-1.0s wall / 12.4-12.9s CPU on a quiet machine (and 1.5s / 20s in the
  interleaved runs above, which looked load-affected). The hand-written decoder (`pbf_decoder.rs`: pooled per-thread
  block buffer, string table as byte ranges with lazily-created `Arc<str>`, packed arrays decoded on the fly while
  iterating, 8-byte varint reads, block structure validated up front) plus libdeflate for zlib (whole-buffer, ~1.5x
  faster than zlib-rs through flate2) took single-threaded decoding of all 8004 blocks from 8.3s to 4.3s
  (decompression from 1.8s to 1.15s). The rest of the CPU time goes to building `Tags` (`String` keys, SipHash
  `IndexMap`), `Arc<str>` refcounts, and the benchmark's per-node `encode_flat_location` (sin + ln).
  The lazy (mmap) and eager read modes cost about the same in sys time.
- LoopLineMerger and Douglas-Peucker are slower than Java; candidates for a geometry perf pass (allocation in
  inner loops, `Vec<Coord>` copies, hashing of node keys).

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
