# Shared helpers for bench/run_all.sh. Source this file; do not execute it.
#
# Every trial is written to $OUT/<bench>/<side>.<trial>.{log,time,meta}:
#   log  - combined stdout+stderr of the benchmark
#   time - `/usr/bin/time -l` rusage (wall, user, sys, peak RSS) of the benchmark process
#   meta - `uptime` before the run, quiet-wait duration, command line, cwd, exit status

RUST_ROOT="${RUST_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
REF_ROOT="${REF_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/planetiler-ref/planetiler}"
BENCH_DATA="${BENCH_DATA:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/planetiler-bench}"
SOURCES="${SOURCES:-$BENCH_DATA/data/sources}"
JAVA="${JAVA:-/opt/homebrew/opt/openjdk@25/bin/java}"
DIST_JAR="${DIST_JAR:-$REF_ROOT/planetiler-dist/target/planetiler-dist-0.10.3-SNAPSHOT-with-deps.jar}"
BENCH_JAR="${BENCH_JAR:-$REF_ROOT/planetiler-benchmarks/target/planetiler-benchmarks-0.10.3-SNAPSHOT.jar}"
JCP="$DIST_JAR:$BENCH_JAR"
JPKG="com.onthegomap.planetiler.benchmarks"
RBIN="$RUST_ROOT/target/release"
SCRATCH="${SCRATCH:-/tmp/ptbench}"

# quiet-machine thresholds
QUIET_LOAD="${QUIET_LOAD:-6}"           # 1-min load must be below this (idle desktop baseline is ~4)
QUIET_MAX_WAIT="${QUIET_MAX_WAIT:-600}" # seconds; give up waiting after this and note it in meta
QUIET_SETTLE="${QUIET_SETTLE:-5}"       # seconds to sleep after the previous run before checking

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

load1() { sysctl -n vm.loadavg | awk '{print $2}'; }

# Names of processes that would disturb a measurement (builds, other JVMs, other planetiler/benchmark runs).
busy_procs() {
  ps -Ao comm= | awk -F/ '{print $NF}' |
    grep -E '^(cargo|rustc|java|planetiler|benchmark_[a-z_]+|long_long_map_bench|generate_openmaptiles|cc|ld|clang)$' |
    sort | uniq -c | tr -s ' ' | tr '\n' ',' || true
}

# Wait until no busy processes run and the 1-minute load average is below QUIET_LOAD. Sets QUIET_NOTE.
wait_quiet() {
  sleep "$QUIET_SETTLE"
  local start=$SECONDS procs l
  while true; do
    procs=$(busy_procs)
    l=$(load1)
    if [[ -z "$procs" ]] && awk -v l="$l" -v t="$QUIET_LOAD" 'BEGIN{exit !(l < t)}'; then
      QUIET_NOTE="quiet after $((SECONDS - start))s (load1=$l)"
      return 0
    fi
    if ((SECONDS - start > QUIET_MAX_WAIT)); then
      QUIET_NOTE="NOT QUIET: gave up after $((SECONDS - start))s (load1=$l procs=$procs)"
      log "$QUIET_NOTE"
      return 0
    fi
    sleep 5
  done
}

# run_trial <bench> <side> <trial> <cwd> <cmd...>
# Runs one trial under /usr/bin/time -l after waiting for a quiet machine.
# Optional env: STOP_REGEX + STOP_COUNT: kill the benchmark once STOP_COUNT log lines match STOP_REGEX (for Java
# benchmarks that loop forever or have a hard-coded round count); HARD_TIMEOUT (seconds, default 3600).
run_trial() {
  local bench=$1 side=$2 trial=$3 cwd=$4
  shift 4
  local dir="$OUT/$bench"
  mkdir -p "$dir"
  local base="$dir/$side.$trial"
  wait_quiet
  {
    echo "uptime: $(uptime)"
    echo "quiet: $QUIET_NOTE"
    echo "cwd: $cwd"
    echo "cmd: $*"
    echo "start: $(date '+%Y-%m-%d %H:%M:%S')"
  } >"$base.meta"
  log "[$bench] $side trial $trial ($QUIET_NOTE)"
  local status
  # every output line is prefixed with a unix timestamp (seconds, ms precision) so per-round times can be derived
  # even when a program only prints coarse durations
  (
    cd "$cwd" || exit 1
    /usr/bin/time -l -o "$base.time" "$@" 2>&1 </dev/null |
      perl -MTime::HiRes=time -ne 'BEGIN{$|=1} printf "%.3f %s", time, $_' >"$base.log"
  ) &
  local tpid=$! t0=$SECONDS killed="" n
  while kill -0 "$tpid" 2>/dev/null; do
    n=0
    [[ -n "${STOP_REGEX:-}" ]] && n=$(grep -c -E "$STOP_REGEX" "$base.log" 2>/dev/null)
    if [[ -n "${STOP_REGEX:-}" ]] && ((${n:-0} >= STOP_COUNT)); then
      killed="stopped after $STOP_COUNT lines matching '$STOP_REGEX'"
    elif ((SECONDS - t0 > ${HARD_TIMEOUT:-3600})); then
      killed="HARD TIMEOUT after ${HARD_TIMEOUT:-3600}s"
    fi
    if [[ -n "$killed" ]]; then
      # /usr/bin/time's child is the benchmark; killing it (not time) lets time still write rusage
      local timepid
      timepid=$(pgrep -P "$tpid" -x time | head -1)
      [[ -n "$timepid" ]] && pkill -TERM -P "$timepid" 2>/dev/null
      sleep 2
      [[ -n "$timepid" ]] && pkill -KILL -P "$timepid" 2>/dev/null
      break
    fi
    sleep 0.5
  done
  wait "$tpid"
  status=$?
  {
    echo "end: $(date '+%Y-%m-%d %H:%M:%S')"
    echo "exit: $status"
    [[ -n "$killed" ]] && echo "killed: $killed"
  } >>"$base.meta"
  if [[ -z "$killed" && $status -ne 0 ]]; then
    log "[$bench] $side trial $trial FAILED (exit $status), see $base.log"
  fi
  return 0
}

# alternate <bench> <trials> <java-cwd> <rust-cwd>
# Requires functions java_cmd and rust_cmd to be defined (they receive the trial number). Runs J R J R ...
# Optional JAVA_STOP_REGEX/JAVA_STOP_COUNT and RUST_STOP_REGEX/RUST_STOP_COUNT, and after_trial (hook run after each).
alternate() {
  local bench=$1 trials=$2 jcwd=$3 rcwd=$4 t
  for ((t = 1; t <= trials; t++)); do
    if [[ -n "${JAVA_ONLY_SKIP:-}" ]]; then :; else
      STOP_REGEX="${JAVA_STOP_REGEX:-}" STOP_COUNT="${JAVA_STOP_COUNT:-0}" \
        run_trial "$bench" java "$t" "$jcwd" $(java_cmd "$t")
      declare -F after_trial >/dev/null && after_trial java "$t"
    fi
    STOP_REGEX="${RUST_STOP_REGEX:-}" STOP_COUNT="${RUST_STOP_COUNT:-0}" \
      run_trial "$bench" rust "$t" "$rcwd" $(rust_cmd "$t")
    declare -F after_trial >/dev/null && after_trial rust "$t"
  done
  return 0
}
