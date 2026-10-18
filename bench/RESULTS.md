# Results

Machine: Apple M5 Pro, 15 cores, 48 GB RAM, macOS (Darwin 25.6). Java: OpenJDK 25.0.4.1 (Homebrew), planetiler
`d88cf89`. Rust: 1.99.0 `--release`, this repo at `627d901` (mimalloc global allocator, the default). Measured
2026-10-18 with `bench/run_all.sh` (sequential, quiet machine, Java and Rust trials alternated). Every number below is
reproducible with one command; the full generated per-benchmark tables (all metrics, min/max, load at each trial) are
in [`results-detail.md`](results-detail.md). Java-vs-Rust output parity for Switzerland:
[`docs/switzerland-parity.md`](../docs/switzerland-parity.md).

## Summary

- **End-to-end OpenMapTiles, Switzerland (549 MB): Rust 10.7s vs Java 37.4s wall (3.49x faster), 121s vs 313s CPU
  (2.58x less, i.e. 61% less), 4.02 GB vs 9.62 GB peak RSS (58% less).** Output 363 MB vs 366 MB, same 23,922 tiles.
- **Monaco: Rust 1.63s vs Java 14.1s wall (8.6x), 11.0s vs 20.2s CPU (1.84x less, 46% less), 0.83 GB vs 1.02 GB
  peak RSS.** Rust's peak RSS is now below Java's on Monaco too (it was 1.20 GB at `cb984fd`; Natural Earth sqlite
  connections now unmap after each table).
- Since the previous quiet pass (`cb984fd`: Rust 14.9s / 168s CPU / 4.03 GB on Switzerland): 28% less wall and 28%
  less CPU. osm_pass2 7.06s -> 3.89s (99s -> 55s CPU), osm_pass1 1.47s -> 0.80s, sort 0.78s -> 0.54s, archive 3.68s ->
  3.41s. Nothing on the end-to-end path got slower (lake_centerlines 0.083s -> 0.090s and water_polygons 0.35s ->
  0.354s are within noise).
- **The JTS buffer / offset-curve port has no measurable end-to-end cost.** Its micro-benchmarks were ~16-21% slower
  per Boston tile, but Switzerland archive z13 (dominated by building `merge_nearby_polygons`) is 2.0s / 28s CPU in
  every trial of both passes, and the archive stage as a whole got faster.
- Rust wins **every stage** of the pipeline: pass1 3.0x, pass2 2.6x, sort 2.5x, archive 2.5x, and 7-14x on the
  simple sources (which are 90% of Java's Monaco time).
- Component benchmarks: Rust is faster on everything Planetiler uses, typically 1.2-5x. **One regression vs the last
  pass:** `BenchmarkPmtiles` fell from 110M to 52M entries/s (now 1.20x Java instead of 2.6x), bisected to `da514d1`
  (PMTiles directory varints moved to `util::var_int`); it only affects `.pmtiles` output. Rows where Rust is slower
  than Java: Douglas-Peucker on 50-100 point rings at tolerance 0.1-1 (0.88-0.97x), and JTS `VWSimplifier` vs geo's
  `SimplifyVw` at tolerance 1-20 on 10-50 points (0.44-0.92x; different algorithms, not on any Planetiler code path).
- Correctness: Monaco output is identical to Java in every layer including feature IDs. Switzerland: identical tile
  set and per-layer feature counts in all 16 layers; normalized differing features are building 578, landuse 35,
  landcover 17, transportation 10 (Java-only = Rust-only in each), plus 1,730 housenumber feature IDs; everything else
  identical. OsmReader `emit` feature counts match exactly (10,665,941 both). See the parity doc.

## Methodology

- `bench/run_all.sh` runs every benchmark in sequence; `bench/summarize.py <out dir>` produces the tables. Raw logs (one
  per trial, every line timestamped), `/usr/bin/time -l` rusage and per-trial metadata (`uptime`, quiet-wait result,
  command, exit status) go to `bench/out/<run>/` (git-ignored). The last trial's e2e outputs are kept as
  `bench/out/<run>/e2e_<name>/{java,rust}.mbtiles` for correctness checks.
- This run (`bench/out/20261018-054434`): `TRIALS=6 bench/run_all.sh osm_read osm_reader`, then `TRIALS=3
  bench/run_all.sh` into the same output directory, after a single `cargo build --release --workspace --bins`
  (nothing built during measurement). The second command ran all benchmarks, so it re-ran `osm_read` / `osm_reader`
  trials 1-3 (about 25 minutes after the first run) and replaced those files; the tables use those trials 1-3 plus
  trials 4-6 from the first command. The pure first-run 6-trial medians differ by at most 1% (e.g. `osm_read` pass
  0.585s vs 0.587s, OsmReader `emit` pass 2 1.73s vs 1.72s).
- Quiet check before every trial: no other `cargo`/`rustc`/`java`/`planetiler`/benchmark processes, and 1-minute load
  average below 6 (the machine's idle baseline is ~2-5 from desktop apps); give up after 10 minutes and flag the
  trial. All 129 trials in the output directory started quiet (load1 at trial start 1.1-6.0, median 4.4); none hit
  the timeout. The J/R alternation cancels the steady background load out of the ratios.
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
  Wall / CPU / peak RSS are the process's `/usr/bin/time -l` numbers. Stage wall times come from line timestamps
  (start of a stage to the start of the next); stage CPU from planetiler's rounded stage summary.
- The MbtilesRead input (500k z14 tiles x 800 random bytes, plain `tiles` table) is generated deterministically by
  `bench/gen_bench_mbtiles.py` into `/tmp/ptbench/bench.mbtiles` when missing.

## End-to-end OpenMapTiles

| Run | Java wall | Java CPU | Java peak RSS | Rust wall | Rust CPU | Rust peak RSS | Wall speedup |
|-----|-----------|----------|---------------|-----------|----------|---------------|--------------|
| Switzerland (`-Xmx16g`) | 37.4s [35.6-37.4] | 313s [296-313] | 9.62 GB | 10.7s [10.3-10.8] | 121s [121-123] | 4.02 GB | 3.49x |
| Monaco (`-Xmx8g`) | 14.1s [14.0-14.3] | 20.2s [20.1-20.4] | 1.02 GB | 1.63s [1.62-2.38] | 11.0s [10.9-11.0] | 0.83 GB | 8.63x |

Output: Switzerland 365,977,600 bytes (Java) vs 362,565,632 (Rust), 23,922 tiles each (identical per zoom); Monaco
311 kB vs 303 kB.

Per-stage wall time (median of 3; from log timestamps) and CPU (planetiler's stage summary, rounded), with the
previous pass (`cb984fd`) for comparison:

| Stage | Switzerland Java | Switzerland Rust | (prev Rust) | speedup | Java CPU | Rust CPU | (prev) | share of Rust wall | Monaco Java | Monaco Rust |
|-------|------------------|------------------|-------------|---------|----------|----------|--------|--------------------|-------------|-------------|
| lake_centerlines | 0.91s | 0.090s | 0.083s | 10x | 2s | 0.5s | 0.5s | 0.8% | 0.89s | 0.084s |
| water_polygons | 4.83s | 0.354s | 0.35s | 13.6x | 5s | 5s | 5s | 3.3% | 4.61s | 0.372s |
| natural_earth | 7.29s | 1.06s | 1.08s | 6.9x | 10s | 6s | 6s | 9.9% | 7.16s | 0.765s |
| osm_pass1 | 2.41s | 0.80s | 1.47s | 3.0x | 15s | 7s | 7s | 7.5% | 0.19s | 0.075s |
| osm_pass2 | 10.2s | 3.89s | 7.06s | 2.61x | 144s | 55s | 99s | 36.4% | 0.24s | 0.13s |
| boundaries + agg_stop + ne_lakes | 0.22s | 0.14s | 0.14s | 1.6x | 0.4s | 0.1s | 0.1s | 1.3% | ~0 | ~0 |
| sort | 1.36s | 0.54s | 0.78s | 2.49x | 9s | 3s | 2s | 5.1% | 0.016s | 0.001s |
| archive | 8.62s | 3.41s | 3.68s | 2.53x | 121s | 45s | 48s | 31.9% | 0.25s | 0.031s |
| **overall (process)** | 37.4s | 10.7s | 14.9s | 3.49x | 313s | 121s | 168s | | 14.1s | 1.63s |

OSM sub-phases and archive zooms (Switzerland, trial 3; planetiler's `Finished ...` lines, wall / CPU; Rust OSM wall
from line timestamps):

| Phase | Java | Rust | (prev Rust) |
|-------|------|------|-------------|
| pass1 nodes | 2s / 9s (avg 5.1 cores) | 0.8s / 5s (5.5) | 0.9s / 5s |
| pass1 ways | 0.4s / 5s (12.8) | 0.17s / 2s (12.8) | 0.5s / 3s (4.8) |
| pass1 relations | 0.2s / 1s | 0.1s / 0.1s | 0.1s / 0.1s |
| pass2 nodes | 2s / 25s | 0.63s / 9s | 1.3s / 19s |
| pass2 ways | 6s / 84s | 2.41s / 35s | 4.7s / 69s |
| pass2 relations | 3s / 39s | 0.82s / 11s | 1.0s / 14s |
| archive z0-z12 | 1.7s (from timestamps) | 0.45s / ~6s | 0.58s |
| archive z13 | 5s / 71s | 2.0s / 28s | 2.0s / 28s |
| archive z14 | 2s / 23s | 0.76s / 11s | 0.8s / 11s |

Notes:
- `water_polygons` is 0.35s of wall but still 5s of CPU (avg 14 cores): the parallel inflate plus 14 workers for a
  stage Java runs on one thread in 5s. Not a wall-time issue on this machine, but it is the least CPU-efficient stage
  (Monaco's 11s CPU is mostly the three simple sources).
- `sort` uses slightly more CPU than before (3s vs 2s) because large sorted groups are now written as several chunks
  in parallel; its wall went down 31%.
- Startup (JVM + config) is ~0.5s of Java's process wall; Rust's is ~0.

## Component benchmarks

Medians over steady-state rounds of 3 processes per side (6 for OsmRead / OsmReader). Higher is better for rates,
lower for times.

| Benchmark | Java | Rust | Rust speedup |
|-----------|------|------|--------------|
| BenchmarkTileCoord (z0-z14 encode+decode) | 831M/s | 1.3B/s | 1.56x |
| BenchmarkVarInt (100M varints) | 190M/s | 268M/s | 1.41x |
| BenchmarkMbtilesWriter (1M tiles x10, avg / best rep) | 3.05M/s / 3.15M/s | 4.08M/s / 4.17M/s | 1.34x / 1.32x |
| BenchmarkMbtilesRead (500k random z14 reads, avg / best rep) | 580k/s / 588k/s | 700k/s / 704k/s | 1.21x / 1.20x |
| BenchmarkPmtiles (60M dir entries round trip) | 43.5M/s (6.9s wall, 27.8s CPU, 10.1 GB RSS) | **52M/s** (3.8s wall, 3.8s CPU, 3.5 GB) | 1.20x (1.81x wall); was 110M/s, see History |
| LongLongMap sparsearray/ram 100M, 8 readers: write / read | 204M/s / 105M/s | 264M/s / 242M/s | 1.29x / 2.30x |
| LongLongMap sparsearray/mmap: write / read | 125M/s / 120M/s | 297M/s / 252M/s | 2.38x / 2.10x |
| LongLongMap array/mmap: write / read | 467M/s / 156M/s | 625M/s / 612M/s | 1.34x / 3.92x |
| VectorTile (bench/java, 5000 iters): encodeGeometry | 12.7M features/s | 36.2M features/s | 2.85x |
| VectorTile: full 3-layer tile encode | 1.64k tiles/s | 4.11k tiles/s | 2.50x |
| BenchmarkOsmRead (OsmReader pass 1), Switzerland: wall | 0.861s | 0.587s [0.580-0.605] | 1.47x |
| BenchmarkOsmRead: CPU | ~11s | 6.20s [6.11-6.44] | ~1.8x |
| OsmReader `--profile=null` Switzerland: pass1 wall / CPU | 1.75s / 10.2s | 0.772s / 6.39s | 2.27x / 1.60x |
| OsmReader `--profile=null`: pass2 wall / CPU | 1.07s / 10.6s | 0.556s / 6.10s | 1.93x / 1.73x |
| OsmReader `--profile=emit`: pass1 wall / CPU | 1.81s / 10.4s | 0.771s / 6.31s | 2.35x / 1.64x |
| OsmReader `--profile=emit`: pass2 wall / CPU | 3.97s / 55.0s | 1.72s / 24.3s | 2.31x / 2.26x |
| TileArchiveWriter (2M features, 37,975 tiles): archive wall / CPU | 1.05s / 14.6s | 0.44s / 5.88s | 2.39x / 2.48x |
| TileArchiveWriter setup: render / sort wall | 2.60s / 1.04s | 0.63s / 0.26s | 4.13x / 4.00x |
| LineSplitter ms (reused 10/100/1000 pts) | 36 / 12 / 6 | 30 / 6 / 2 | 1.20x / 2.0x / 3.0x |
| LineSplitter ms (!reused 10/100/1000) | 76 / 45 / 38 | 54 / 17 / 12 | 1.41x / 2.65x / 3.17x |
| LineSplitter ms (cacheable 10/100/1000) | 39 / 12 / 6 | 33 / 7 / 3 | 1.18x / 1.71x / 2.0x |
| LineMerge JTS LineMerger, 200k/240k-line inputs (ms) | 260 / 316 | 67.5 / 75.0 | 3.85x / 4.21x |
| LineMerge JTS LineMerger rates (1759-pt, 50x2, 10x10, 2x50 /s) | 82k / 54k / 310k / 1M | 131k / 220k / 827k / 2.5M | 1.6-4.1x |
| LineMerge loop(0), 200k/240k-line inputs (ms) | 79.1 / 81.7 | 13.8 / 15.8 | 5.73x / 5.17x |
| LineMerge loop(0) rates (/s) | 12k / 108k / 281k / 407k | 23k / 345k / 713k / 943k | 1.9-3.2x |
| LineMerge loop(0.1) / loop(20) | | | 1.9-5.0x / 1.8-3.8x |
| Simplify DP(1) (2/10/50/100/10k-pt circles, /s) | 41M / 13M / 4.5M / 2.4M / 25k | 43M / 16M / 4.2M / 2.3M / 25k | 1.05 / 1.23 / **0.93 / 0.96** / 1.0x |
| Simplify DP(0.1), DP(20) | | | 1.0-1.33x (**0.97 / 0.88x** at 50/100 pts), 1.02-1.28x |
| Simplify VW(0) (Planetiler's VW) | 12M / 4.5M / 1M / 522k / 4k | 18M / 8.3M / 2.3M / 1.1M / 5.4k | 1.50 / 1.84 / 2.30 / 2.11 / 1.35x |
| Simplify VW(0.1 / 1 / 20) | | | 1.50-1.92x small, 1.06-1.07x at 10k pts |
| Simplify JTS VWSimplifier (Java) vs geo SimplifyVw (Rust), tol 1 / 20 | 20M / 2M / 650k / 175k / 14 | 23M / 10M / 601k / 255k / 850 | different algorithms; geo **0.44x / 0.64x** at tol 20 on 10/50 pts |
| benchmark_simple_readers (Rust only): water polygons read+reproject | (proxy: Java `water_polygons` stage 4.8s) | 0.454s | - |
| benchmark_simple_readers: Natural Earth unzip / total | (proxy: Java `natural_earth` stage 7.3s) | 0.57s / 0.84s | - |

The Simplify DP rows at 50-100 points moved by one unit of the benchmark's 2-significant-digit output (DP(0.1) 100
pts: Rust 1.6M -> 1.5M/s, Java 1.7M both passes); no simplification code changed between the passes.

Process-level numbers (wall, CPU, peak RSS per benchmark process) are in `results-detail.md`; Rust's peak RSS is lower
everywhere in the component benchmarks (e.g. 0.94 vs 5.5 GB VarInt, 0.8 vs 6.0 GB array/mmap LongLongMap, 4.9 vs
10.6 GB TileArchiveWriter, 2.5 vs 6.7 GB OsmReader emit, 0.82 vs 3.1 GB OsmRead).

Correctness cross-checks seen in the logs: VectorTile hashes match (`8512da4e8a2d855c` both); TileArchiveWriter writes
37,975 tiles on both sides (256.6 MB vs 255.5 MB file); OsmReader `--profile=emit` emits 10,665,941 features on both
sides.

### Bimodality check (OSM decoding)

Per-process steady-state values (one per process, median of its steady-state passes/iterations), Rust:

| Benchmark | 6 processes | spread |
|-----------|-------------|--------|
| `benchmark_osm_read` pass wall | 581, 582, 586, 588, 588, 589 ms | unimodal |
| `benchmark_osm_read` pass CPU | 6.11-6.44s (all 24 steady-state passes) | unimodal |
| OsmReader `null` pass 2 wall | 0.542-0.562s (all 12 steady-state samples) | unimodal |

`benchmark_osm_read` now runs the real `OsmReader::pass1` like Java's BenchmarkOsmRead (`09073c3`); the previous
pass's 0.479s was the older pass-1 emulation, so the two are not comparable (Java: 0.85-0.86s both times).

## History (before/after of notable optimizations)

- **Previous quiet-machine pass (`cb984fd`, 2026-10-18 02:23) -> this pass (`627d901`).** Same machine, same harness.
  - Switzerland Rust 14.9s / 168s CPU / 4.03 GB -> 10.7s / 121s / 4.02 GB (wall and CPU both 28% less); vs Java
    2.50x -> 3.49x wall. osm_pass1 1.47s -> 0.80s (ways sub-phase 0.5s at 4.8 cores -> 0.17s at 12.8), osm_pass2
    7.06s -> 3.89s (99s -> 55s CPU; ways 4.7s / 69s -> 2.4s / 35s, nodes 1.3s / 19s -> 0.6s / 9s, relations
    1.0s / 14s -> 0.8s / 11s), sort 0.78s -> 0.54s, archive 3.68s -> 3.41s (48s -> 45s CPU). Simple sources
    unchanged within noise.
  - Monaco 2.02s / 11.0s CPU / 1.20 GB -> 1.63s / 11.0s / 0.83 GB (natural_earth 1.12s -> 0.77s; RSS from the
    Natural Earth sqlite unmapping).
  - Rust Switzerland output 362,479,616 -> 362,565,632 bytes (JTS buffer port; the parity numbers moved accordingly).
  - Components: OsmReader `null` pass 2 0.635s -> 0.556s, `emit` pass 2 2.26s / 31.3s -> 1.72s / 24.3s CPU;
    TileArchiveWriter setup render 2.21s -> 0.63s, sort 0.38s -> 0.26s, archive 0.46s -> 0.44s; MbtilesWriter
    3.90M -> 4.08M/s. Regression: PMTiles 110M -> 52M/s (next item).
- **PMTiles directory regression (`da514d1`).** `benchmark_pmtiles` (which only round-trips 60M entries through
  `directory_to_bytes` / `directory_from_bytes`) went from 105-111M/s to 48-52M/s. Bisected with fresh builds, 3 runs
  each, same machine: `da514d1^` 105-107M/s, `da514d1` 49-52M/s, `627d901` 51-52M/s. That commit replaced pmtiles'
  local varint loops with `util::var_int::put_var_long` / `try_get_var_long` (the only change on this path), so the
  shared varint helpers are the cause; not profiled further. Only `.pmtiles` output is affected (still 1.20x Java).
- **JTS buffer / offset-curve port and cascaded union (Fix B) end to end.** Micro-benchmarks put the new buffer at
  ~16-21% slower per Boston tile than the `geo`/`i_overlay` version, but on Switzerland the stage it affects is flat:
  archive z13 (building `merge_nearby_polygons`) 2.0s / 28s CPU and z14 0.8s / 11s in all 3 trials of both passes.
  Parity improved a lot (building differing features 2,387 -> 578, landuse 404 -> 35; see the parity doc).
- **OSM pass 2 / pass 1 / sort / archive / RSS pass after `cb984fd` (Rust only; the quiet-machine tables above,
  at `627d901`, include these changes).** Interleaved A/B, `cb984fd` vs `c91867b`, 4 runs each on a shared
  machine (load 8-15 from other builds; medians [min-max], stage wall from log timestamps, CPU = user+sys of the
  process). Output byte-identical (`cmp` of the Switzerland and Monaco mbtiles; `planetiler compare`: 0 tile diffs).
  - Switzerland: 15.8s [15.4-17.0] / 177s CPU -> 11.8s [11.5-12.5] / 134s CPU, peak RSS 4.0 GB both. osm_pass1
    1.35s -> 0.91s (ways sub-phase 0.6s at 4.7 cores -> 0.2s at 12.6), osm_pass2 7.54s -> 4.57s (100-104s -> 59-63s
    CPU; ways 5s / 69s CPU -> 3s / 38s, nodes 18s -> 10s CPU, relations 15s -> 12s CPU), sort 0.84s -> 0.62s,
    archive 4.13s -> 3.83s (52s -> 49s CPU).
  - Monaco: 1.70s / 11.2s CPU / 1.20 GB -> 1.65s / 11.1s / 0.84 GB peak RSS.
  - Pass 2 hotspots found with `sample` (Switzerland, ways phase): the shared attribute-key `CommonStringEncoder`
    (`RwLock` + SipHash, every rendered feature) was ~17% of pass 2 CPU -> per-encoder-thread caches; the
    multipolygon-way multimap's global lock (one lock per way, ~4s CPU spinning) -> per-thread batches flushed
    before the relations phase; the single temp-feature writer thread spent 2/3 of its time in `write()`, so
    processing threads blocked on its queue -> a background I/O thread per chunk file; snap rounding built an
    R-tree of hot pixels for every tile polygon -> sorted arrays; multipolygon ring nesting ran a full prepared
    DE-9IM relate per candidate ring -> vertex location when the (valid) rings' edges do not meet; SipHash in the
    MultiExpression indexes and `MatchAny` -> foldhash; the mountain_peak US test recomputed the Natural Earth US
    bounding box per peak. Pass 1: ways were preprocessed by the single node-writer thread -> on the parse threads
    (after all earlier blocks finished preprocessing nodes).
  - Archive: building `merge_nearby_polygons` is ~86% of z13 encode time (buffer/union in `geo`/`i_overlay`, not
    changed here); the proximity grouping skips self-distances, computes each pair once and shortcuts pairs that
    share a vertex or contain each other's first vertex (~15% -> ~12% of z12-z14 encode samples). Command decoding and the
    housenumber Java-HashMap-order sort were tightened.
  - Monaco RSS: the parallel inflater's in-flight chunks were not the main cost (capping them to 4-8 chunks saves
    0.1-0.16 GB but slows water_polygons linearly, 0.38s -> 0.62s at 8 and 0.94s at 5). Most of the 1.2 GB peak
    was the Natural Earth sqlite file: every reader connection kept its memory map until all tables were read, so
    the whole ~850 MB file was resident at the end; connections now unmap after each table (same speed).

- **Older quiet-machine pass (2026-10-17, `c116961`, system malloc, before the simple-source and allocator
  changes; superseded by the tables above).** Switzerland: Rust 26.6s [26.2-27.0] / 205s CPU / 5.81 GB vs Java 37.1s / 306s / 9.63 GB (1.39x).
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

Ranked by expected impact on end-to-end time (Switzerland Rust: 10.7s wall / 121s CPU; Monaco 1.63s / 11s). Every
stage is faster than Java, so these are about absolute time, not parity.

1. **osm_pass2 (3.89s, 36% of wall; 55s CPU; 2.6x Java).** Ways 2.4s / 35s CPU, relations 0.8s / 11s, nodes 0.6s /
   9s. The reader alone is 0.56s / 6.1s CPU (OsmReader `null`) and `emit` (rendering every element as a trivial
   feature) 1.72s / 24s, so roughly 30s of the 55s CPU is OpenMapTiles profile work and rendering real geometries.
   Next: `sample` the ways phase again now that the encoder/lock hotspots are gone, to split profile matching, way
   geometry construction, `FeatureRenderer` clipping/simplification and the feature-group writer.
2. **archive (3.41s, 32%; 45s CPU at 14 cores; 2.5x Java).** z13 is 2.0s / 28s CPU of it (building
   `merge_nearby_polygons`: buffer / cascaded union, now the JTS port), z14 0.76s / 11s, z0-z12 0.45s. The buffer port
   is ~16-21% slower than the old `geo` buffer in micro-benchmarks without showing up here, so profiling its
   offset-curve and noding paths is the most direct lever; gzip via libdeflate is the next candidate.
3. **natural_earth (1.06s, 10% of Switzerland but 47% of Monaco's 1.63s).** Mostly unzipping the sqlite database
   from the 434 MB zip (`benchmark_simple_readers`: 0.57s unzip of 0.84s). Caching the extracted sqlite between runs
   (keyed on the zip's size/mtime) would bring Monaco to ~1s.
4. **osm_pass1 (0.80s, 7.5%; 3.0x Java).** The nodes sub-phase is 0.8s at avg 5.5 cores (ways now run at 12.8); its
   parallelism is the remaining limit, worth up to ~0.4s.
5. **sort (0.54s, 5%; avg 4.8 cores).** Read 0.29s / write 0.89s / sort 0.15s summed over threads; the write side
   dominates.
6. **PMTiles directory varints** (regression above): restore the specialized loops or speed up `util::var_int`.
   No end-to-end effect for `.mbtiles` output, but `.pmtiles` writes pay it on every directory.
7. **Efficiency, not wall time:** water_polygons burns 5s CPU for 0.35s wall (avg 14 cores; Java uses 5s on one
   thread). Smaller: Douglas-Peucker on 50-100 point lines at tolerance 0.1-1 (0.88-0.97x Java).

### History: util::var_int fast path (after f60f636)

The cleanup pass (`da514d1`) switched PMTiles directory varints to `util::var_int`, whose per-byte `split_first` +
`i == 9` branch halved `BenchmarkPmtiles` (110M -> 52M entries/s). `try_get_var_long` now has a single-byte fast path
and index-based decoding, and `put_var_long` uses a `while v >= 0x80` loop. Spot check on a quiet machine (load ~1.6,
Rust only, not a full pass): BenchmarkPmtiles 95-109M entries/s, BenchmarkVarInt 362-450M/s (was 244M/s). Re-measure
against Java in the next full pass.
