#!/usr/bin/env bash
# Runs the Java vs Rust benchmark suite sequentially on a quiet machine, alternating Java and Rust trials
# (J R J R J R), then summarizes the results.
#
# Usage:
#   cargo build --release --workspace --bins      # build once, never during measurement
#   bench/run_all.sh                              # everything
#   bench/run_all.sh tile_coord e2e_monaco        # a subset (names: see BENCHES below)
#   TRIALS=5 OUT=/tmp/ptbench/results bench/run_all.sh var_int
#   python3 -I bench/summarize.py <out dir>       # re-summarize existing results
#
# Raw logs go to $OUT (default bench/out/<timestamp>, git-ignored); summary.md is written there at the end.
# See bench/lib.sh for the quiet-machine check and per-trial files. Paths are configurable via env (lib.sh).
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TRIALS="${TRIALS:-3}"
OUT="${OUT:-$RUST_ROOT/bench/out/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT" "$SCRATCH"
SWISS="$BENCH_DATA/switzerland-latest.osm.pbf"
MONACO="$BENCH_DATA/monaco-latest.osm.pbf"
JBENCH="$RUST_ROOT/bench/java"

reset_hooks() {
  unset -f after_trial
  unset JAVA_STOP_REGEX JAVA_STOP_COUNT RUST_STOP_REGEX RUST_STOP_COUNT
}

# ---------------------------------------------------------------- component benchmarks

bench_tile_coord() {
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkTileCoord"; }
  rust_cmd() { echo "$RBIN/benchmark_tile_coord"; }
  alternate tile_coord "$TRIALS" "$SCRATCH" "$SCRATCH"
}

bench_var_int() {
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkVarInt"; }
  rust_cmd() { echo "$RBIN/benchmark_var_int"; }
  alternate var_int "$TRIALS" "$SCRATCH" "$SCRATCH"
}

bench_mbtiles_writer() {
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkMbtilesWriter"; }
  rust_cmd() { echo "$RBIN/benchmark_mbtiles_writer"; }
  alternate mbtiles_writer "$TRIALS" "$SCRATCH" "$SCRATCH"
}

bench_mbtiles_read() {
  # input: 500k z14 tiles x 800 random bytes, generated deterministically (see gen_bench_mbtiles.py)
  local mb="$SCRATCH/bench.mbtiles"
  if [[ ! -f "$mb" ]]; then
    log "generating $mb"
    python3 -I "$RUST_ROOT/bench/gen_bench_mbtiles.py" "$mb"
  fi
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkMbtilesRead --bench_mbtiles=$SCRATCH/bench.mbtiles"; }
  rust_cmd() { echo "$RBIN/benchmark_mbtiles_read --bench_mbtiles=$SCRATCH/bench.mbtiles"; }
  alternate mbtiles_read "$TRIALS" "$SCRATCH" "$SCRATCH"
}

bench_pmtiles() {
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkPmtiles"; }
  rust_cmd() { echo "$RBIN/benchmark_pmtiles"; }
  alternate pmtiles "$TRIALS" "$SCRATCH" "$SCRATCH"
}

bench_long_long_map() {
  local cfg
  for cfg in "sparsearray ram" "sparsearray mmap" "array mmap"; do
    LLM_CFG="$cfg"
    java_cmd() { echo "$JAVA -cp $JCP $JPKG.LongLongMapBench $LLM_CFG 100000000 8"; }
    rust_cmd() { echo "$RBIN/long_long_map_bench $LLM_CFG 100000000 8"; }
    mkdir -p "$SCRATCH/llmap"
    alternate "long_long_map_${cfg// /_}" "$TRIALS" "$SCRATCH/llmap" "$SCRATCH/llmap"
    rm -rf "$SCRATCH/llmap"
  done
}

bench_vector_tile() {
  java_cmd() { echo "$JAVA -cp $DIST_JAR $JBENCH/BenchmarkVectorTile.java 5000"; }
  rust_cmd() { echo "$RBIN/benchmark_vector_tile 5000"; }
  alternate vector_tile "$TRIALS" "$SCRATCH" "$SCRATCH"
}

bench_osm_read() {
  # Java loops forever: stop it after 6 passes. Passes 1-2 are discarded as warmup on both sides.
  JAVA_STOP_REGEX=$'^[0-9.]+ (\e\\[[0-9;]*m)*[0-9.hms]+ cpu:[0-9.hms]+ avg:' JAVA_STOP_COUNT=6
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkOsmRead --osm_path=$SWISS"; }
  rust_cmd() { echo "$RBIN/benchmark_osm_read --osm_path=$SWISS --iterations=6 --precise=true"; }
  alternate osm_read "$TRIALS" "$SCRATCH" "$SCRATCH"
  reset_hooks
}

bench_osm_reader() {
  local p
  for p in null emit; do
    OSMR_PROFILE=$p
    java_cmd() {
      echo "$JAVA -Xmx16g -cp $DIST_JAR $JBENCH/BenchmarkOsmReader.java --osm_path=$SWISS --profile=$OSMR_PROFILE" \
        "--iterations=3 --tmpdir=$SCRATCH/osm_reader_java"
    }
    rust_cmd() {
      echo "$RBIN/benchmark_osm_reader --osm_path=$SWISS --profile=$OSMR_PROFILE --iterations=3" \
        "--tmpdir=$SCRATCH/osm_reader_rust"
    }
    alternate "osm_reader_$p" "$TRIALS" "$SCRATCH" "$SCRATCH"
  done
  rm -rf "$SCRATCH/osm_reader_java" "$SCRATCH/osm_reader_rust"
}

bench_tile_archive_writer() {
  mkdir -p "$SCRATCH/taw"
  java_cmd() { echo "$JAVA -Xmx16g -cp $DIST_JAR $JBENCH/BenchmarkTileArchiveWriter.java 2000000 5 $SCRATCH/taw"; }
  rust_cmd() { echo "$RBIN/benchmark_tile_archive_writer 2000000 5 $SCRATCH/taw"; }
  alternate tile_archive_writer "$TRIALS" "$SCRATCH" "$SCRATCH"
  rm -rf "$SCRATCH/taw"
}

bench_line_splitter() {
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkLineSplitter"; }
  rust_cmd() { echo "$RBIN/benchmark_line_splitter"; }
  alternate line_splitter "$TRIALS" "$SCRATCH" "$SCRATCH"
}

bench_line_merge() {
  # Java hard-codes 10 rounds (~25s each); stop after 4 (16 rows). Rust runs 4 via BENCH_ITERS. Round 1 discarded.
  JAVA_STOP_REGEX=$'^[0-9.]+ (\e\\[[0-9;]*m)* *(JTS|loop\()' JAVA_STOP_COUNT=16
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkLineMerge"; }
  rust_cmd() { echo "env BENCH_ITERS=4 $RBIN/benchmark_line_merge"; }
  # resources are read from planetiler-core/src/test/resources relative to the reference repo root
  alternate line_merge "$TRIALS" "$REF_ROOT" "$REF_ROOT"
  reset_hooks
}

bench_simplify() {
  # Java hard-codes 10 rounds (~55s each); stop after 4 (44 rows). Rust runs 4 via BENCH_ITERS. Round 1 discarded.
  JAVA_STOP_REGEX=$'^[0-9.]+ (\e\\[[0-9;]*m)* *(DP|JTS VW|VW)\(' JAVA_STOP_COUNT=44
  java_cmd() { echo "$JAVA -cp $JCP $JPKG.BenchmarkSimplify"; }
  rust_cmd() { echo "env BENCH_ITERS=4 $RBIN/benchmark_simplify"; }
  alternate simplify "$TRIALS" "$SCRATCH" "$SCRATCH"
  reset_hooks
}

bench_simple_readers() {
  # Rust only; Java's water_polygons / natural_earth stage times from the end-to-end logs are the proxy.
  local t
  for ((t = 1; t <= TRIALS; t++)); do
    run_trial simple_readers rust "$t" "$SCRATCH" "$RBIN/benchmark_simple_readers" \
      "--water=$SOURCES/water-polygons-split-3857.zip" \
      "--natural_earth=$SOURCES/natural_earth_vector.sqlite.zip" "--tmp=$SCRATCH/simple_readers"
    rm -rf "$SCRATCH/simple_readers"
  done
}

# ---------------------------------------------------------------- end-to-end OpenMapTiles

# e2e <name> <pbf> <xmx>
e2e() {
  local name=$1 pbf=$2 xmx=$3
  E2E_NAME=$name E2E_PBF=$pbf E2E_XMX=$xmx
  mkdir -p "$SCRATCH/e2e/work-java" "$SCRATCH/e2e/work-rust"
  java_cmd() {
    echo "$JAVA -Xmx$E2E_XMX -jar $DIST_JAR --download_dir=$SOURCES --osm_path=$E2E_PBF" \
      "--output=$SCRATCH/e2e/java-$E2E_NAME.mbtiles --force"
  }
  rust_cmd() {
    echo "$RBIN/planetiler --download_dir=$SOURCES --osm_path=$E2E_PBF" \
      "--output=$SCRATCH/e2e/rust-$E2E_NAME.mbtiles --force"
  }
  after_trial() {
    local f="$SCRATCH/e2e/$1-$E2E_NAME.mbtiles"
    echo "output_bytes: $(stat -f %z "$f" 2>/dev/null || echo missing)" >>"$OUT/e2e_$E2E_NAME/$1.$2.meta"
    rm -f "$f" "$f-journal" "$f-wal" "$f-shm"
    rm -rf "$SCRATCH/e2e/work-$1/data/tmp"
  }
  alternate "e2e_$name" "$TRIALS" "$SCRATCH/e2e/work-java" "$SCRATCH/e2e/work-rust"
  reset_hooks
}

bench_e2e_monaco() { e2e monaco "$MONACO" 8g; }
bench_e2e_switzerland() { e2e switzerland "$SWISS" 16g; }

# ---------------------------------------------------------------- main

BENCHES=(tile_coord var_int mbtiles_writer mbtiles_read pmtiles long_long_map vector_tile osm_read osm_reader
  tile_archive_writer line_splitter line_merge simplify simple_readers e2e_monaco e2e_switzerland)
selected=("$@")
((${#selected[@]})) || selected=("${BENCHES[@]}")

for b in "${selected[@]}"; do
  declare -F "bench_$b" >/dev/null || { echo "unknown benchmark: $b (known: ${BENCHES[*]})" >&2; exit 1; }
done
[[ -x "$RBIN/planetiler" ]] || { echo "build first: cargo build --release --workspace --bins" >&2; exit 1; }

{
  echo "date: $(date)"
  echo "host: $(sysctl -n machdep.cpu.brand_string) cores=$(sysctl -n hw.ncpu) mem=$(($(sysctl -n hw.memsize) / 1073741824))GB"
  echo "java: $("$JAVA" -version 2>&1 | head -1)"
  echo "rustc: $(rustc --version 2>/dev/null)"
  echo "rust commit: $(git -C "$RUST_ROOT" rev-parse --short HEAD)"
  echo "ref commit: $(git -C "$REF_ROOT" rev-parse --short HEAD 2>/dev/null)"
  echo "trials: $TRIALS"
  echo "benches: ${selected[*]}"
} >>"$OUT/env.txt"

for b in "${selected[@]}"; do
  log "=== $b"
  reset_hooks
  "bench_$b"
done

python3 -I "$RUST_ROOT/bench/summarize.py" "$OUT" >"$OUT/summary.md" && log "summary: $OUT/summary.md"
