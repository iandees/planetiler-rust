# Results

Machine: Apple M5 Pro, 15 cores, 48 GB RAM, macOS (Darwin 25.6). Java: OpenJDK 25.0.4.1 (Homebrew), planetiler
`d88cf89`. Rust: 1.99.0 `--release`, this repo at `c116961`. Measured 2026-10-17 with `bench/run_all.sh`
(sequential, quiet machine, Java and Rust trials alternated). Every number below is reproducible with one command; the
full generated per-benchmark tables (all metrics, min/max, load at each trial) are in
[`results-detail.md`](results-detail.md).

## Summary

- **End-to-end OpenMapTiles, Switzerland (549 MB): Rust 26.6s vs Java 37.1s wall (1.39x faster), 205s vs 306s CPU
  (1.49x less), 5.8 GB vs 9.6 GB peak RSS.** Output 364 MB vs 366 MB.
- **Monaco: Rust 11.6s vs Java 14.4s wall (1.24x), but 22.7s vs 20.6s CPU (Rust uses 10% more CPU)**, RSS ~equal
  (0.95 vs 0.99 GB). Monaco is dominated by the global water-polygon and Natural Earth sources.
- Rust wins every OSM/tile stage of the pipeline (pass1 1.6x, pass2 1.36x, sort 1.9x, archive 1.8x). It **loses
  `water_polygons`** (5.4s vs 4.7-4.9s, 0.89x) and spends **1.6x the CPU in `natural_earth`** (16s vs 10s, though
  1.4x faster wall).
- Component benchmarks: Rust is faster on almost everything, typically 1.3-4x (LongLongMap reads 2.4-3.9x, PMTiles
  3.4x, LoopLineMerger 2-4.5x, vector tile encoding 2.4-2.5x, tile archive writing 1.9x). Still slower: Douglas-Peucker
  on 50-100 point rings (0.87-0.96x), LineSplitter on 10-point lines (0.92x), and the `null`-profile OSM reader pass 2
  (0.70x wall, 0.54x CPU).
- **Rust's multithreaded OSM decoding is bimodal**: in ~3 out of 4 processes the same work (identical instruction
  count) runs at IPC ~1.16 instead of ~2.9, i.e. 2.4x the CPU time (see "Where to optimize next" #1). Java does not
  show this.

## Methodology

- `bench/run_all.sh` runs every benchmark in sequence; `bench/summarize.py <out dir>` produces the tables. Raw logs (one
  per trial, every line timestamped), `/usr/bin/time -l` rusage and per-trial metadata (`uptime`, quiet-wait result,
  command, exit status) go to `bench/out/<run>/` (git-ignored).
- Quiet check before every trial: no other `cargo`/`rustc`/`java`/`planetiler`/benchmark processes, and 1-minute load
  average below the threshold (give up after 10 minutes and flag the trial). The first part of this run used load < 3;
  the machine's idle baseline turned out to be ~2.5-4 from desktop apps (WindowServer, Firefox video decoding,
  coreaudiod), so from `vector_tile` onwards the threshold was 6 (now the default). All 111 trials started quiet
  (load1 at trial start 1.7-6.0, median 2.9); none hit the 10-minute timeout. The J/R alternation cancels the steady
  background load out of the ratios.
- Each benchmark runs **J R J R J R** (3 trials per side, each its own process). Tables give median [min-max] over all
  steady-state samples of all trials, and "Rust speedup" = Java/Rust for times (Rust/Java for rates), so >1 means Rust is
  faster; **bold** means Rust is slower.
- Warmup handling (same rule on both sides): drop round 1 where a benchmark prints rounds (TileCoord, VarInt, PMTiles:
  rounds 2-3; LineMerge, Simplify: rounds 2-4; LineSplitter: rounds 3-10); BenchmarkOsmRead: passes 3-6 of 6;
  OsmReader: iterations 2-3 of 3; TileArchiveWriter: archive rounds 1-4 of 0-4; MbtilesRead's own 3 pre-warm reps are
  excluded by the program; MbtilesWriter reports aggregate stats over its 10 reps (Java's include JIT warmup) and
  LongLongMap a single write and 30s read phase, so those are as printed. Java benchmarks with a hard-coded 10 rounds
  (LineMerge, Simplify) or an infinite loop (BenchmarkOsmRead) are stopped by the harness after the rounds needed; Rust
  runs the same number of rounds via `BENCH_ITERS` / `--iterations`. Java's BenchmarkOsmRead only prints
  1-significant-digit durations, so its per-pass wall time is taken from line timestamps.
- JVM flags: defaults (max heap = 12 GB) for component benchmarks, `-Xmx16g` for OsmReader / TileArchiveWriter and the
  Switzerland run, `-Xmx8g` for Monaco. Threads: defaults on both sides (all 15 cores).
- End-to-end: `java -Xmx<N> -jar planetiler-dist-*-with-deps.jar` vs `target/release/planetiler`, both with
  `--download_dir=../planetiler-bench/data/sources --osm_path=<pbf> --output=/tmp/ptbench/e2e/<side>-<x>.mbtiles
  --force`, each from its own empty working directory (tmp files under `data/tmp`). Sources are cached, so no downloads.
  Stage times come from line timestamps (start of a stage to the start of the next); the rounded stage summary planetiler
  prints is in `results-detail.md`.
- The MbtilesRead input (500k z14 tiles x 800 random bytes, plain `tiles` table) is generated deterministically by
  `bench/gen_bench_mbtiles.py` into `/tmp/ptbench/bench.mbtiles` when missing.

## End-to-end OpenMapTiles

| Run | Java wall | Java CPU | Java peak RSS | Rust wall | Rust CPU | Rust peak RSS | Wall speedup |
|-----|-----------|----------|---------------|-----------|----------|---------------|--------------|
| Switzerland (`-Xmx16g`) | 37.1s [36.6-38.2] | 306s [300-324] | 9.63 GB | 26.6s [26.2-27.0] | 205s [204-210] | 5.81 GB | 1.39x |
| Monaco (`-Xmx8g`) | 14.4s [14.3-14.6] | 20.6s [20.1-20.6] | 0.99 GB | 11.6s [11.5-11.7] | 22.7s [22.7-22.7] | 0.95 GB | 1.24x |

Output: Switzerland 365,977,600 bytes (Java) vs 364,273,664 (Rust); Monaco 311 kB vs 307 kB.

Per-stage wall time (median of 3; from log timestamps) and CPU (planetiler's stage summary, rounded):

| Stage | Switzerland Java | Switzerland Rust | speedup | Java CPU | Rust CPU | Monaco Java | Monaco Rust |
|-------|------------------|------------------|---------|----------|----------|-------------|-------------|
| lake_centerlines | 1.00s | 0.48s | 2.1x | 2s | 0.5s | 0.99s | 0.46s |
| water_polygons | 4.85s | 5.43s | **0.89x** | 5s | 5s | 4.69s | 5.37s |
| natural_earth | 7.28s | 5.30s | 1.38x | 10s | **16s** | 7.25s | 5.33s |
| osm_pass1 | 2.42s | 1.53s | 1.58x | 16s | 8s | 0.20s | 0.09s |
| osm_pass2 | 10.5s | 7.73s | 1.36x | 145s | 107s | 0.23s | 0.14s |
| boundaries + agg_stop + ne_lakes | 0.23s | 0.17s | 1.3x | 0.6s | 0.1s | ~0 | ~0 |
| sort | 1.36s | 0.73s | 1.87x | 9s | 2s | 0.02s | 0.002s |
| archive | 8.60s | 4.85s | 1.77x | 119s | 65s | 0.24s | 0.05s |
| **overall** | 36.5s | 27s | 1.35x | 302s | 205s | 14s | 12s |

Notes:
- Rust's stage summary reports `ne_lakes 6s cpu:1m8s`, but the log timeline shows it takes ~0s: the stage timer
  overlaps osm_pass2 (the stage starts in `Water::finish`, which runs inside the pass-2 workers). It is a stats
  accounting artifact, not real time; worth fixing so the summary is trustworthy.
- Both implementations unzip `natural_earth_vector.sqlite.zip` **twice** per run (2.2s each; same behavior in Java).
- Startup (JVM + config) is ~0.5s of Java's process wall; Rust's is ~0.

## Component benchmarks

Medians over steady-state rounds of 3 processes per side. Higher is better for rates, lower for times.

| Benchmark | Java | Rust | Rust speedup |
|-----------|------|------|--------------|
| BenchmarkTileCoord (z0-z14 encode+decode) | 848M/s | 1.4B/s | 1.65x |
| BenchmarkVarInt (100M varints) | 181M/s | 244M/s | 1.35x |
| BenchmarkMbtilesWriter (1M tiles x10, avg / best rep) | 3.04M/s / 3.16M/s | 3.87M/s / 3.95M/s | 1.27x / 1.25x |
| BenchmarkMbtilesRead (500k random z14 reads, avg / best rep) | 552k/s / 568k/s | 599k/s / 642k/s | 1.09x / 1.13x |
| BenchmarkPmtiles (60M dir entries round trip) | 32M/s (8.1s wall, 28.9s CPU, 10.1 GB RSS) | 109M/s (2.1s wall, 2.1s CPU, 3.2 GB) | 3.4x |
| LongLongMap sparsearray/ram 100M, 8 readers: write / read | 203M/s / 101M/s | 273M/s / 238M/s | 1.34x / 2.36x |
| LongLongMap sparsearray/mmap: write / read | 128M/s / 107M/s | 290M/s / 267M/s | 2.27x / 2.50x |
| LongLongMap array/mmap: write / read | 455M/s / 154M/s | 629M/s / 605M/s | 1.38x / 3.93x |
| VectorTile (bench/java, 5000 iters): encodeGeometry | 12.9M features/s | 32.8M features/s | 2.54x |
| VectorTile: full 3-layer tile encode | 1.68k tiles/s | 3.98k tiles/s | 2.37x |
| BenchmarkOsmRead pass1-emulation, Switzerland: wall | 0.85s | 1.39s (bimodal: 0.57s or 1.39s) | **0.61x** (1.5x in fast mode) |
| BenchmarkOsmRead: CPU | ~11s | 19.1s (7.6s or 19.1s) | **0.58x** (1.4x in fast mode) |
| OsmReader `--profile=null` Switzerland: pass1 wall / CPU | 1.74s / 10.0s | 1.59s / 12.9s | 1.09x / **0.77x** |
| OsmReader `--profile=null`: pass2 wall / CPU | 1.04s / 10.2s | 1.48s / 18.9s | **0.70x / 0.54x** |
| OsmReader `--profile=emit`: pass1 wall / CPU | 1.77s / 10.3s | 1.32s / 9.8s | 1.34x / 1.04x |
| OsmReader `--profile=emit`: pass2 wall / CPU | 3.89s / 53.6s | 2.61s / 36.1s | 1.49x / 1.49x |
| TileArchiveWriter (2M features, 37,975 tiles): archive wall / CPU | 1.04s / 14.5s | 0.56s / 7.7s | 1.87x / 1.87x |
| TileArchiveWriter setup: render / sort wall | 2.79s / 1.12s | 2.93s / 0.40s | **0.95x** / 2.80x |
| LineSplitter ms (reused 10/100/1000 pts) | 36 / 12 / 6 | 39 / 10 / 3 | **0.92x** / 1.2x / 2.0x |
| LineSplitter ms (!reused 10/100/1000) | 75 / 45 / 38 | 66 / 23 / 13 | 1.14x / 1.96x / 2.9x |
| LineSplitter ms (cacheable 10/100/1000) | 40 / 12 / 7 | 43 / 11 / 4 | **0.93x** / 1.09x / 1.75x |
| LineMerge JTS LineMerger, 200k/240k-line inputs (ms) | 275 / 344 | 84.5 / 91.3 | 3.25x / 3.77x |
| LineMerge JTS LineMerger rates (1759-pt, 50x2, 10x10, 2x50 /s) | 81k / 53k / 309k / 1.0M | 118k / 130k / 444k / 1.3M | 1.3-2.5x |
| LineMerge loop(0), 200k/240k-line inputs (ms) | 83.2 / 85.7 | 18.5 / 22.5 | 4.5x / 3.8x |
| LineMerge loop(0) rates (/s) | 11k / 107k / 274k / 393k | 23k / 273k / 601k / 864k | 2.1-2.6x |
| LineMerge loop(0.1) / loop(20) | | | 2.0-4.3x / 2.0-3.5x |
| Simplify DP(1) (2/10/50/100/10k-pt circles, /s) | 40M / 13M / 4.5M / 2.3M / 25k | 40M / 14M / 3.9M / 2.2M / 25k | 1.0 / 1.08 / **0.87 / 0.96** / 1.0x |
| Simplify DP(0.1), DP(20) | | | 0.88-1.25x, 0.92-1.11x (slower at 50/100 pts) |
| Simplify VW(0) (Planetiler's VW) | 12M / 4.4M / 1M / 512k / 3k | 15M / 7.2M / 1.8M / 1M / 5.9k | 1.25 / 1.64 / 1.80 / 1.95 / 1.97x |
| Simplify VW(0.1 / 1 / 20) | | | 1.25-1.78x small, 1.06-1.08x at 10k pts |
| Simplify JTS VWSimplifier (Java) vs geo SimplifyVw (Rust), tol 1 | 20M / 2M / 643k / 170k / 14 | 16M / 6.7M / 567k / 246k / 828 | different algorithms (geo skips JTS's topology repair) |
| benchmark_simple_readers (Rust only): water polygons read+reproject | (proxy: Java `water_polygons` stage 4.7-4.9s) | 2.84s | - |
| benchmark_simple_readers: Natural Earth unzip / total | (proxy: Java `natural_earth` stage 7.3s) | 2.16s / 2.94s | - |

Process-level numbers (wall, CPU, peak RSS per benchmark process) are in `results-detail.md`; Rust's peak RSS is lower
everywhere (e.g. 1.2 vs 5.5 GB VarInt, 0.8 vs 6.0 GB array/mmap LongLongMap, 6.4 vs 9.8 GB TileArchiveWriter, 2.6 vs
6.7 GB OsmReader emit), partly because the JVM sizes its heap generously.

Correctness cross-checks seen in the logs: VectorTile hashes match (`8512da4e8a2d855c` both); TileArchiveWriter writes
37,975 tiles on both sides (256.6 MB vs 255.5 MB file); OsmReader `--profile=emit` emits **10,665,941 (Java) vs
10,665,942 (Rust)** features: one extra feature in Rust, worth tracking down.

### The bimodal OSM-decoding slowdown

`benchmark_osm_read` (and to a lesser degree OsmReader pass 2 with the null profile) runs in one of two modes per
process, fixed for the process lifetime:

| Mode | pass wall | pass CPU | instructions retired (6 passes) | IPC |
|------|-----------|----------|---------------------------------|-----|
| fast | 0.57s | 7.7s | 567G | 2.9 |
| slow | 1.39s | 19.1s | 569G | 1.16 |

Same instruction count, 2.5x the cycles. 8 extra back-to-back runs gave 2 fast / 6 slow; with `--threads=4` it is 4 fast
(IPC 3.6) / 1 slow (IPC 2.4); with `MallocNanoZone=0` most runs land in a third mode (0.78s, IPC 2.2). Java (IPC 2.7,
no variance) is unaffected. That points to a memory-system effect that depends on the per-process address layout:
allocator behavior (macOS system malloc with cross-thread frees of `Arc<str>`/`Tags`/block buffers) or false sharing /
cache-set aliasing between per-thread hot data. The 3 trials in the main table landed 1 fast / 2 slow, which is why the
median looks bad.

## History (before/after of notable optimizations)

Older numbers from the provisional (loaded-machine) pass, kept only to show what changed; the tables above supersede
them.
- Simple sources (after the tables above were measured; Rust only, interleaved A/B on a busy machine, load 7-15,
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

Ranked by expected impact on end-to-end time (Switzerland Rust: 26.6s wall / 205s CPU; Monaco 11.6s).

1. **The bimodal IPC problem in OSM decoding (osm_pass1/osm_pass2, ~9.3s of Switzerland wall, 115s CPU).** In fast
   mode `benchmark_osm_read` is 1.5x faster than Java; in slow mode (most processes) it is 0.6x. If the e2e passes
   are hit the same way, fixing it is worth a large share of pass-2's 107s CPU. Next steps: A/B a different global
   allocator (mimalloc/jemalloc) in `benchmark_osm_read` and `planetiler`, look for shared hot cache lines (pad
   per-worker state with `CachePadded`, avoid cross-thread `Arc` refcount traffic on shared strings), and use
   Instruments' CPU counters on a slow-mode process.
2. **osm_pass2 (7.7s, 29% of wall; 1.36x Java).** The largest stage. The ways phase alone is ~5s at 14 threads
   (74s CPU); the `null`-profile OsmReader shows Rust's pass-2 overhead without a profile is 0.54x Java's CPU, so the
   reader plumbing (node-location lookups, way geometry building, `Tags` construction, channel hand-off to the writer)
   is costlier than Java's even though the OpenMapTiles profile work is cheaper.
3. **water_polygons (5.4s, 20% of wall; 0.89x Java — the one stage Rust loses).** It is single-threaded (CPU = wall).
   The read+reproject part alone is 2.84s (`benchmark_simple_readers`); parallelize shapefile record decoding /
   reprojection across workers (Java's reader is also 1 thread but faster), and profile the remaining ~2.5s of
   feature processing/writing. This stage also dominates Monaco-sized runs.
4. **natural_earth (5.3s, 20% of wall; 1.38x faster but 1.6x the CPU).** 2.2s of it is unzipping the sqlite file,
   and it is unzipped twice (same in Java): extract once and reuse (saves ~2.2s wall on every run, ~8% of Switzerland,
   ~19% of Monaco). Then find where the extra 6s of CPU goes (likely workers busy-waiting or redundant geometry work
   in the process step: `process 14x(10%)`).
5. **archive (4.85s, 18%; 1.77x Java).** Already well ahead; next gains are in z13/z14 tile encoding (3s of the 5s).
6. **osm_pass1 (1.5s; 1.58x).** The pass-1 "process" step is a single thread at 99% busy; the ways sub-phase runs
   at avg 3.8 cores. Parallelizing way-membership bookkeeping would cut ~0.5-0.8s.
7. Smaller: sort (0.73s), `render` in TileArchiveWriter setup (0.95x Java), Douglas-Peucker on 50-100 point
   lines (0.87-0.96x), LineSplitter on 10-point lines (0.92x). Also fix the `ne_lakes` stage-timer overlap so the
   stage summary is accurate.
