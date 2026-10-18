# Results

Machine: Apple M5 Pro, 15 cores, 48 GB RAM, macOS (Darwin 25.6). Java: OpenJDK 25.0.4.1 (Homebrew), planetiler
`d88cf89`. Rust: 1.99.0 `--release`, this repo at `cb984fd` (mimalloc global allocator, the default). Measured
2026-10-18 with `bench/run_all.sh` (sequential, quiet machine, Java and Rust trials alternated). Every number below is
reproducible with one command; the full generated per-benchmark tables (all metrics, min/max, load at each trial) are
in [`results-detail.md`](results-detail.md). Java-vs-Rust output parity for Switzerland:
[`docs/switzerland-parity.md`](../docs/switzerland-parity.md).

## Summary

- **End-to-end OpenMapTiles, Switzerland (549 MB): Rust 14.9s vs Java 37.2s wall (2.50x faster), 168s vs 310s CPU
  (1.84x less), 4.0 GB vs 9.5 GB peak RSS.** Output 362 MB vs 366 MB, same 23,922 tiles.
- **Monaco: Rust 2.0s vs Java 14.0s wall (6.9x), 11.0s vs 20.1s CPU (1.83x less)**, but peak RSS is now higher in
  Rust (1.20 vs 1.02 GB; the parallel zip inflater keeps ~16 1 MB chunks in flight).
- Since the previous pass (Rust 26.6s / 205s CPU / 5.8 GB on Switzerland): the simple sources went from 11.2s to 1.5s
  of wall (water_polygons 5.4s -> 0.35s, natural_earth 5.3s -> 1.1s, lake_centerlines 0.48s -> 0.08s), and mimalloc
  took ~10% off osm_pass2 CPU, ~19% off archive CPU and 1.4 GB off peak RSS (A/B below).
- Rust now wins **every stage** of the pipeline: pass1 1.7x, pass2 1.5x, sort 1.7x, archive 2.35x, and 6-14x on the
  simple sources (which are 81% of Java's Monaco time).
- **The bimodal OSM-decoding slowdown is gone.** `benchmark_osm_read`: 6/6 Rust processes at 474-533 ms per pass
  (6.3-6.5s CPU), 1.77x faster than Java (was 0.57s or 1.39s depending on the process); OsmReader `null`-profile
  pass 2 0.61-0.67s in all 6 (was 1.48s median, 0.70x Java; now 1.68x faster).
- Component benchmarks: Rust is faster on everything that Planetiler uses, typically 1.2-5x. The only remaining rows
  where Rust is slower are Douglas-Peucker on 50-100 point rings at tolerance 0.1-1 (0.94-0.97x), and JTS
  `VWSimplifier` vs geo's `SimplifyVw` at tolerance 1-20 on 10-50 points (0.46-0.94x; different algorithms, not on
  any Planetiler code path).
- Correctness: OsmReader `emit` feature counts now match exactly (10,665,941 both); Switzerland outputs have identical
  tile sets and per-layer feature counts except one extra z13 building in Rust (an area-4.002 px² polygon at the
  4 px² merge threshold); see the parity doc.

## Methodology

- `bench/run_all.sh` runs every benchmark in sequence; `bench/summarize.py <out dir>` produces the tables. Raw logs (one
  per trial, every line timestamped), `/usr/bin/time -l` rusage and per-trial metadata (`uptime`, quiet-wait result,
  command, exit status) go to `bench/out/<run>/` (git-ignored). The last trial's e2e outputs are kept as
  `bench/out/<run>/e2e_<name>/{java,rust}.mbtiles` for correctness checks.
- This run: `TRIALS=6 bench/run_all.sh osm_read osm_reader`, then `TRIALS=3 bench/run_all.sh` for everything else,
  into one output directory, after a single `cargo build --release --workspace --bins` (nothing built during
  measurement).
- Quiet check before every trial: no other `cargo`/`rustc`/`java`/`planetiler`/benchmark processes, and 1-minute load
  average below 6 (the machine's idle baseline is ~2.5-4 from desktop apps); give up after 10 minutes and flag the
  trial. All 129 trials started quiet (load1 at trial start 1.1-6.0, median 4.9); none hit the timeout. The J/R
  alternation cancels the steady background load out of the ratios.
- Each benchmark runs **J R J R J R** (3 trials per side, each its own process; 6 per side for `osm_read` and
  `osm_reader`). Tables give median [min-max] over all steady-state samples of all trials, and "Rust speedup" =
  Java/Rust for times (Rust/Java for rates), so >1 means Rust is faster; **bold** means Rust is slower.
- Warmup handling (same rule on both sides): drop round 1 where a benchmark prints rounds (TileCoord, VarInt, PMTiles:
  rounds 2-3; LineMerge, Simplify: rounds 2-4; LineSplitter: rounds 3-10); BenchmarkOsmRead: passes 3-6 of 6;
  OsmReader: iterations 2-3 of 3; TileArchiveWriter: archive rounds 1-4 of 0-4; MbtilesRead's own 3 pre-warm reps are
  excluded by the program; MbtilesWriter reports aggregate stats over its 10 reps (Java's include JIT warmup) and
  LongLongMap a single write and 30s read phase, so those are as printed. Java benchmarks with a hard-coded 10 rounds
  (LineMerge, Simplify) or an infinite loop (BenchmarkOsmRead) are stopped by the harness after the rounds needed
  (the 12 `exit 143` trials); Rust runs the same number of rounds via `BENCH_ITERS` / `--iterations`. Java's
  BenchmarkOsmRead only prints 1-significant-digit durations, so its per-pass wall time is taken from line timestamps.
- JVM flags: defaults (max heap = 12 GB) for component benchmarks, `-Xmx16g` for OsmReader / TileArchiveWriter and the
  Switzerland run, `-Xmx8g` for Monaco. Threads: defaults on both sides (all 15 cores).
- End-to-end: `java -Xmx<N> -jar planetiler-dist-*-with-deps.jar` vs `target/release/planetiler`, both with
  `--download_dir=../planetiler-bench/data/sources --osm_path=<pbf> --output=/tmp/ptbench/e2e/<side>-<x>.mbtiles
  --force`, each from its own empty working directory (tmp files under `data/tmp`). Sources are cached, so no downloads.
  Stage wall times come from line timestamps (start of a stage to the start of the next); stage CPU from planetiler's
  rounded stage summary.
- The MbtilesRead input (500k z14 tiles x 800 random bytes, plain `tiles` table) is generated deterministically by
  `bench/gen_bench_mbtiles.py` into `/tmp/ptbench/bench.mbtiles` when missing.

## End-to-end OpenMapTiles

| Run | Java wall | Java CPU | Java peak RSS | Rust wall | Rust CPU | Rust peak RSS | Wall speedup |
|-----|-----------|----------|---------------|-----------|----------|---------------|--------------|
| Switzerland (`-Xmx16g`) | 37.2s [37.2-37.6] | 310s [303-311] | 9.54 GB | 14.9s [14.8-14.9] | 168s [168-169] | 4.03 GB | 2.50x |
| Monaco (`-Xmx8g`) | 14.0s [14.0-14.2] | 20.1s [20.0-20.2] | 1.02 GB | 2.02s [1.98-2.93] | 11.0s [11.0-11.1] | **1.20 GB** | 6.95x |

Output: Switzerland 365,977,600 bytes (Java) vs 362,479,616 (Rust), 23,922 tiles each (identical per zoom); Monaco
311 kB vs 303 kB.

Per-stage wall time (median of 3; from log timestamps) and CPU (planetiler's stage summary, rounded):

| Stage | Switzerland Java | Switzerland Rust | speedup | Java CPU | Rust CPU | share of Rust wall | Monaco Java | Monaco Rust |
|-------|------------------|------------------|---------|----------|----------|--------------------|-------------|-------------|
| lake_centerlines | 0.92s | 0.083s | 11x | 2s | 0.5s | 0.6% | 0.88s | 0.079s |
| water_polygons | 4.79s | 0.35s | 13.7x | 5s | 5s | 2.4% | 4.63s | 0.365s |
| natural_earth | 7.23s | 1.08s | 6.7x | 10s | 6s | 7.3% | 7.09s | 1.12s |
| osm_pass1 | 2.57s | 1.47s | 1.74x | 16s | 7s | 9.9% | 0.21s | 0.078s |
| osm_pass2 | 10.6s | 7.06s | 1.50x | 144s | 99s | 47.6% | 0.24s | 0.13s |
| boundaries + agg_stop + ne_lakes | 0.23s | 0.14s | 1.6x | 0.4s | 0.1s | 1.0% | ~0 | ~0 |
| sort | 1.34s | 0.78s | 1.72x | 9s | 2s | 5.2% | 0.016s | 0.001s |
| archive | 8.65s | 3.68s | 2.35x | 121s | 48s | 24.8% | 0.24s | 0.037s |
| **overall (process)** | 37.2s | 14.9s | 2.50x | 310s | 168s | | 14.0s | 2.02s |

OSM sub-phases (Switzerland, trial 3, planetiler's `Finished nodes/ways/relations` lines, wall / CPU):

| Phase | Java | Rust |
|-------|------|------|
| pass1 nodes | 2s / 9s (avg 4.8 cores) | 0.9s / 5s (5.2) |
| pass1 ways | 0.4s / 6s (14.1) | **0.5s / 3s (4.8)** |
| pass1 relations | 0.2s / 1s | 0.1s / 0.1s |
| pass2 nodes | 2s / 27s | 1.3s / 19s |
| pass2 ways | 6s / 82s | 4.7s / 69s |
| pass2 relations | 3s / 38s | 1.0s / 14s |

Notes:
- `water_polygons` is now 0.35s of wall but still 5s of CPU (avg 14 cores): the parallel inflate plus 14 workers for
  a stage Java runs on one thread in 5s. Not a wall-time issue on this machine, but it is the least CPU-efficient
  stage (Monaco's 11s CPU is mostly the three simple sources).
- `natural_earth` CPU: an earlier optimization agent saw it rise 16s -> 19s with mimalloc. Re-checked after the
  simple-reader changes with an interleaved A/B (Switzerland, `--no-default-features` build = system malloc, 3 runs
  each): natural_earth is 0.8-1s / 5-6s CPU with either allocator, so the regression no longer exists (it was in the
  old per-row decoding path). The same A/B, whole run: mimalloc 14-15s / 167-168s CPU / 4.0 GB vs system malloc
  16s / 191-193s / 5.45 GB (osm_pass2 7s vs 8s, 98-99s vs 107-112s CPU; archive 3s vs 4s, 48s vs 59s CPU).
- Startup (JVM + config) is ~0.5s of Java's process wall; Rust's is ~0.

## Component benchmarks

Medians over steady-state rounds of 3 processes per side (6 for OsmRead / OsmReader). Higher is better for rates,
lower for times.

| Benchmark | Java | Rust | Rust speedup |
|-----------|------|------|--------------|
| BenchmarkTileCoord (z0-z14 encode+decode) | 830M/s | 1.3B/s | 1.57x |
| BenchmarkVarInt (100M varints) | 186M/s | 268M/s | 1.43x |
| BenchmarkMbtilesWriter (1M tiles x10, avg / best rep) | 3.04M/s / 3.17M/s | 3.90M/s / 4.09M/s | 1.28x / 1.29x |
| BenchmarkMbtilesRead (500k random z14 reads, avg / best rep) | 592k/s / 597k/s | 695k/s / 701k/s | 1.17x / 1.17x |
| BenchmarkPmtiles (60M dir entries round trip) | 42.5M/s (7.0s wall, 28.1s CPU, 10.1 GB RSS) | 110M/s (2.1s wall, 2.1s CPU, 3.5 GB) | 2.6x (3.3x wall) |
| LongLongMap sparsearray/ram 100M, 8 readers: write / read | 204M/s / 105M/s | 269M/s / 243M/s | 1.32x / 2.31x |
| LongLongMap sparsearray/mmap: write / read | 107M/s / 120M/s | 282M/s / 254M/s | 2.64x / 2.12x |
| LongLongMap array/mmap: write / read | 425M/s / 154M/s | 623M/s / 614M/s | 1.47x / 3.99x |
| VectorTile (bench/java, 5000 iters): encodeGeometry | 13.7M features/s | 36.3M features/s | 2.64x |
| VectorTile: full 3-layer tile encode | 1.61k tiles/s | 3.98k tiles/s | 2.48x |
| BenchmarkOsmRead pass1-emulation, Switzerland: wall | 0.847s | 0.479s [0.474-0.533] | 1.77x |
| BenchmarkOsmRead: CPU | ~11s | 6.38s [6.27-6.54] | ~1.7x |
| OsmReader `--profile=null` Switzerland: pass1 wall / CPU | 1.77s / 10.3s | 1.13s / 6.80s | 1.57x / 1.51x |
| OsmReader `--profile=null`: pass2 wall / CPU | 1.07s / 10.6s | 0.635s / 6.66s | 1.68x / 1.59x |
| OsmReader `--profile=emit`: pass1 wall / CPU | 1.79s / 10.4s | 1.09s / 6.64s | 1.64x / 1.57x |
| OsmReader `--profile=emit`: pass2 wall / CPU | 4.04s / 55.0s | 2.26s / 31.3s | 1.79x / 1.76x |
| TileArchiveWriter (2M features, 37,975 tiles): archive wall / CPU | 1.06s / 14.6s | 0.46s / 6.04s | 2.30x / 2.42x |
| TileArchiveWriter setup: render / sort wall | 2.65s / 1.11s | 2.21s / 0.38s | 1.20x / 2.92x |
| LineSplitter ms (reused 10/100/1000 pts) | 36 / 12 / 6 | 30 / 6 / 2 | 1.20x / 2.0x / 3.0x |
| LineSplitter ms (!reused 10/100/1000) | 75 / 44 / 38 | 54 / 17 / 12 | 1.39x / 2.59x / 3.17x |
| LineSplitter ms (cacheable 10/100/1000) | 40 / 12 / 6 | 33 / 6.5 / 3 | 1.21x / 1.85x / 2.0x |
| LineMerge JTS LineMerger, 200k/240k-line inputs (ms) | 257 / 324 | 67.4 / 75.6 | 3.81x / 4.29x |
| LineMerge JTS LineMerger rates (1759-pt, 50x2, 10x10, 2x50 /s) | 91k / 54k / 314k / 1.1M | 129k / 218k / 808k / 2.4M | 1.4-4.0x |
| LineMerge loop(0), 200k/240k-line inputs (ms) | 78.3 / 80.8 | 13.8 / 15.9 | 5.67x / 5.08x |
| LineMerge loop(0) rates (/s) | 11k / 108k / 279k / 398k | 23k / 345k / 716k / 947k | 2.1-3.2x |
| LineMerge loop(0.1) / loop(20) | | | 2.1-5.0x / 2.0-3.7x |
| Simplify DP(1) (2/10/50/100/10k-pt circles, /s) | 41M / 14M / 4.5M / 2.4M / 25k | 44M / 16M / 4.3M / 2.3M / 25k | 1.07 / 1.14 / **0.96 / 0.96** / 1.0x |
| Simplify DP(0.1), DP(20) | | | 0.94-1.33x (**0.97 / 0.94x** at 50/100 pts), 1.02-1.28x |
| Simplify VW(0) (Planetiler's VW) | 13M / 4.8M / 1.1M / 574k / 3.9k | 18M / 8.4M / 2.3M / 1.1M / 5.9k | 1.38 / 1.75 / 2.09 / 1.92 / 1.51x |
| Simplify VW(0.1 / 1 / 20) | | | 1.38-1.77x small, 1.06-1.07x at 10k pts |
| Simplify JTS VWSimplifier (Java) vs geo SimplifyVw (Rust), tol 1 / 20 | 20M / 2M / 636k / 137k / 14 | 23M / 10M / 599k / 254k / 846 | different algorithms; geo **0.46x / 0.66x** at tol 20 on 10/50 pts |
| benchmark_simple_readers (Rust only): water polygons read+reproject | (proxy: Java `water_polygons` stage 4.8s) | 0.455s | - |
| benchmark_simple_readers: Natural Earth unzip / total | (proxy: Java `natural_earth` stage 7.2s) | 0.82s / 1.09s | - |

Process-level numbers (wall, CPU, peak RSS per benchmark process) are in `results-detail.md`; Rust's peak RSS is lower
everywhere in the component benchmarks (e.g. 0.94 vs 5.5 GB VarInt, 0.8 vs 6.0 GB array/mmap LongLongMap, 4.9 vs
9.9 GB TileArchiveWriter, 2.45 vs 6.5 GB OsmReader emit, 0.79 vs 3.3 GB OsmRead).

Correctness cross-checks seen in the logs: VectorTile hashes match (`8512da4e8a2d855c` both); TileArchiveWriter writes
37,975 tiles on both sides (256.6 MB vs 255.5 MB file); OsmReader `--profile=emit` emits 10,665,941 features on both
sides (the extra Rust feature from the previous pass is fixed).

### Bimodality check (OSM decoding)

Per-process steady-state values (one per process, median of its steady-state passes/iterations), Rust:

| Benchmark | 6 processes | spread |
|-----------|-------------|--------|
| `benchmark_osm_read` pass wall | 475, 476, 479, 481, 485, 488 ms (single passes 474-533 ms) | unimodal |
| `benchmark_osm_read` pass CPU | 6.33-6.49s | unimodal |
| OsmReader `null` pass 2 wall | 0.614-0.667s (all 12 steady-state samples) | unimodal |
| OsmReader `null` pass 2 CPU | 6.53-6.75s | unimodal |

Previous pass: `benchmark_osm_read` 0.57s/7.7s CPU (fast) or 1.39s/19.1s CPU (slow, ~3 in 4 processes); OsmReader
`null` pass 2 1.20-1.56s / 14.8-19.9s CPU. Java: 0.827-0.895s, unchanged.

## History (before/after of notable optimizations)

Older numbers, kept only to show what changed; the tables above supersede - **Previous quiet-machine pass (2026-10-17, `c116961`, system malloc, before the simple-source and allocator
  changes).** Switzerland: Rust 26.6s [26.2-27.0] / 205s CPU / 5.81 GB vs Java 37.1s / 306s / 9.63 GB (1.39x).
  Monaco: Rust 11.6s / 22.7s CPU vs Java 14.4s / 20.6s (1.24x, Rust used 10% more CPU). Rust stages (Switzerland):
  lake_centerlines 0.48s, water_polygons 5.43s (0.89x Java, the one stage Rust lost), natural_earth 5.30s (16s CPU),
  osm_pass1 1.53s, osm_pass2 7.73s (107s CPU), sort 0.73s, archive 4.85s (65s CPU). Components then slower than Java:
  BenchmarkOsmRead (0.61x, bimodal), OsmReader `null` pass 2 (0.70x wall / 0.54x CPU), TileArchiveWriter render
  (0.95x), LineSplitter on 10-point lines (0.92-0.93x), Douglas-Peucker on 50-100 points (0.87-0.96x), and `emit`
  produced one extra feature. The bimodal OSM decoding looked like this (8 extra back-to-back runs gave 2 fast / 6
  slow; same instruction count, 2.5x the cycles):

  | Mode | pass wall | pass CPU | instructions retired (6 passes) | IPC |
  |------|-----------|----------|---------------------------------|-----|
  | fast | 0.57s | 7.7s | 567G | 2.9 |
  | slow | 1.39s | 19.1s | 569G | 1.16 |
them.
- Simple sources (after the previous pass; Rust only, interleaved A/B on a busy machine, load 7-15,
  3 runs each, medians [min-max] from log timestamps). Changes: readers opened for counting are reused for reading
  (Natural Earth unzipped once; bounds-filtered shapefile records kept from the count pass so the `.shp` is inflated
  once), parallel inflate of large zip entries (`util::parallel_inflate`), Natural Earth rows outside the bounds
  skipped from their WKB envelope before decoding, sqlite read through mmap with memstatus off (the shared page cache
  mutex was the bottleneck), and the `ne_lakes` stage timer is now stopped (it used to run until the end of the run
  and report `6s cpu:1m8s`).
  - Switzerland: total 26.7s [26.3-27.2] -> 18.1s [17.2-18.1]; lake_centerlines 0.48s -> 0.09s, water_polygons
    5.48s [5.45-5.50] -> 0.37s [0.36-0.37], natural_earth 5.24s (16s CPU) -> 0.86s (6s CPU); ne_lakes now ~0s/0s CPU.
    Peak RSS 5.78 -> 5.56 GB. osm_pass1/2 were 1.57/7.84s vs 1.76/8.32s in these runs (code unchanged, see the
    bimodal-IPC note). Output byte-identical.
  - Monaco: total 11.6s [11.55-11.62] -> 1.74s [1.74-1.84]; peak RSS 0.97 -> 1.09 GB (the parallel inflater keeps
    ~16 1 MB chunks in flight). Output byte-identical.
  - `benchmark_simple_readers`: water polygons read 2.80s -> 0.46s; Natural Earth unzip 2.07s -> 0.57s, read 0.68s
    -> 0.25s, total 2.81s -> 0.91s.
- **Bimodal OSM decoding (fixed after the previous pass).** Root cause: false sharing on `Arc<str>` refcounts. The PBF
  decoder hands every element `Arc` clones of per-block strings (the user name for every node, tag values), so each
  element does an atomic increment + decrement on a string its own thread allocated. macOS's allocator (xzone
  malloc) packs those small allocations from different threads into the same 128-byte cache lines, so in most
  processes (depending on placement) the cores fight over lines: `sample` of a slow process showed the element drop
  (refcount decrement) ~27x slower per pass than in a fast one while libdeflate ran at the same speed; giving the user
  string a padded 1 KiB allocation removed the slow mode (0/8 vs 1/8 slow), and a single shared `Arc` for all threads
  was even slower (2.3s/32s). Fix: mimalloc as the global allocator in `planetiler` and every benchmark binary
  (default `mimalloc` feature; thread-local pages), plus foldhash for `Tags` (SipHash was ~6% of decoding). Interleaved
  A/B, Switzerland, steady-state passes, one value per process:
  - `benchmark_osm_read` wall: system malloc 576 / 577 / 579 / 581 / 1386 / 1394 / 1395 / 1412 ms (CPU 7.6-7.8s
    or 18.9-19.1s); another 6 runs were all slow (1.34-1.40s); jemalloc 516-532 ms (6.9s CPU, 8/8); mimalloc
    519-528 ms (6.9-7.0s, 8/8); mimalloc + foldhash 474-486 ms (6.3-6.5s CPU, 10/10).
  - OsmReader `--profile=null` pass 2: 1.20-1.56s wall / 14.8-19.9s CPU -> 0.61-0.65s / 6.4-6.7s (pass 1: 1.38-1.80s /
    10.5-14.5s -> 1.08-1.34s / 6.5-6.9s); `--profile=emit` pass 2: 2.65-2.80s / 36.8-39.0s -> 2.20-2.23s / 30.6-31.0s.
  - End-to-end Switzerland (4 interleaved runs each): 26.6-27.1s wall / 206-215s CPU / 5.8-5.9 GB RSS -> 24.7-26.1s /
    187-189s / 4.2-4.6 GB. Stages: osm_pass1 cpu 9-11s -> 7s, osm_pass2 7.7s -> 7s wall (1m48s -> 1m40s cpu),
    archive 5s -> 4s (65s -> 53s cpu); natural_earth CPU went up 16s -> 19s with mimalloc (same wall).
- The `emit` profile's one extra feature (10,665,942 vs Java's 10,665,941) was relation 9683714, a sliver
  multipolygon whose shell collapses when simplified at z14. JTS keeps the surviving hole as a `LinearRing`, which
  Java renders as a polygon filtered by the unsquared min size and drops; Rust treated it as a closed line and emitted
  it. The renderer now reproduces the `LinearRing` handling (see PORTING.md); the counts match.
- TileCoord: the straight port ran at 604M/s (0.72x Java); closed-form zoom start `(4^z - 1) / 3`, `leading_zeros`,
  shifts/masks instead of `/` and `%` gave 1.3-1.4B/s.
- VectorTile: the straight port ran at 11.6-12.0M features/s and ~1.5k tiles/s (0.86-0.9x Java). Writing protobuf
  bytes directly into an exactly-sized buffer, sharing tag-index `Vec`s per layer, foldhash interning and a
  thread-local geometry command buffer took it to 2.4-2.5x Java.
- OsmRead: the prost-based decoder ran at 0.95-1.0s wall / 12.4-12.9s CPU; the hand-written decoder
  (`pbf_decoder.rs`) plus libdeflate took single-threaded decoding of all 8004 blocks from 8.3s to 4.3s.
- LoopLineMerger was 0.55-0.7x Java before the geometry perf pass (edge-pair coordinate arena, inline node edge lists,
  one hash map, reusable scratch buffers); it is now 2.0-4.5x faster than Java. Douglas-Peucker went from 0.75-1.0x to
  0.87-1.25x (4-lane scan); VW from 0.75x on small inputs to 1.25-1.97x.


## Where to optimize next

Ranked by expected impact on end-to-end time (Switzerland Rust: 14.9s wall / 168s CPU; Monaco 2.0s / 11s). Every
stage is now faster than Java, so these are about absolute time, not parity.

1. **osm_pass2 (7.06s, 48% of wall; 99s CPU; 1.50x Java).** By sub-phase: ways 4.7s / 69s CPU, nodes 1.3s / 19s,
   relations 1.0s / 14s. The reader itself is cheap now (OsmReader `null` pass 2: 0.64s / 6.7s CPU; `emit`, which adds
   feature rendering and writing for every element, 2.26s / 31s), so roughly 65s of the 99s CPU is OpenMapTiles
   profile work and rendering of real geometries (way/multipolygon building, clipping/simplifying into z0-14 tiles).
   Next step: `sample`/Instruments on a Switzerland run during the ways phase to split profile matching, geometry
   construction, `FeatureRenderer` tiling and the feature-group writer.
2. **archive (3.68s, 25%; 48s CPU at 14 cores; 2.35x Java).** Tile post-processing (`merge_nearby_polygons` buffer /
   unbuffer for buildings and landuse, line merging for transportation) plus encoding and gzip of z13/z14 tiles.
   Profile the split between post-processing and encode/compress; gzip is a candidate for libdeflate (already used for
   PBF decoding).
3. **osm_pass1 (1.47s, 10%; 1.74x Java).** The ways sub-phase is the one OSM sub-phase where Rust is slower than Java
   (0.5s at avg 4.8 cores vs 0.4s at 14 cores); nodes run at avg 5.2 cores. Find what limits the ways sub-phase to ~5
   cores (not profiled yet; likely the way-membership / relation bookkeeping); bringing it to Java's parallelism
   could save ~0.3-0.5s.
4. **natural_earth (1.08s, 7% of Switzerland but 55% of Monaco's 2.0s).** 0.5-0.8s of it is unzipping the sqlite
   database from the 434 MB zip (once per run now). For small extracts this is the dominant cost: caching the extracted sqlite between
   runs (keyed on the zip's size/mtime) would bring Monaco to ~1.2s. The stage runs at avg 5.4 cores.
5. **sort (0.78s, 5%; avg 3.1 cores).** Runs at ~3 of 15 cores; not profiled yet.
6. **Efficiency, not wall time:** water_polygons burns 5s CPU for 0.35s wall (avg 14 cores; Java uses 5s on one
   thread), and Monaco's peak RSS (1.20 GB) is now above Java's (1.02 GB) because the parallel inflater keeps ~16 1 MB
   chunks in flight and Natural Earth is extracted in full; bounding the in-flight chunks would fix the RSS.
7. Smaller: Douglas-Peucker on 50-100 point lines at tolerance 0.1-1 (0.94-0.97x Java).
