# Rust vs Java benchmarks

Machine: Apple Silicon, 15 cores, 48 GB RAM, macOS (Darwin 25.6).
Java: OpenJDK 25.0.4.1 (Homebrew), planetiler `d88cf89`.

- `java/` — Java-side benchmark programs that have no upstream equivalent.
- `RESULTS.md` — measured results and methodology; `results-detail.md` — full generated tables.
- `run_all.sh` (+ `lib.sh`) — runs the whole Java vs Rust suite sequentially on a quiet machine, alternating Java and
  Rust trials, then writes `bench/out/<timestamp>/summary.md` via `summarize.py`. Build first
  (`cargo build --release --workspace --bins`), then `bench/run_all.sh` or `bench/run_all.sh <bench>...`
  (`TRIALS`, `QUIET_LOAD`, `OUT` and input paths are configurable via env; see the scripts).
- `gen_bench_mbtiles.py` — deterministic BenchmarkMbtilesRead input (`/tmp/ptbench/bench.mbtiles`).

Component benchmarks are ports of `planetiler-benchmarks` with identical workloads:

```sh
# Rust
cargo run --release -p planetiler-benchmarks --bin <name> -- <args>
# Java
java -cp ../planetiler-ref/planetiler/planetiler-benchmarks/target/*-with-deps.jar com.onthegomap.planetiler.benchmarks.<Name> <args>
```

End-to-end runs use the same input extract and profile on both sides; see `RESULTS.md` for commands.
