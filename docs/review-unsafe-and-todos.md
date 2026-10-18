# `unsafe` audit and remaining `TODO(port)`s

Current as of `main` @ `8ccc4e4` (2026-10-18). The original read-only review was done at `c116961`; this document
keeps only what is still relevant. Fix history is in git (`git log -- docs/review-unsafe-and-todos.md` and the commits
it references).

## `unsafe` code

### Fixed findings from the audit at `c116961`

| Finding | Fix |
|---|---|
| **Unsound:** `custommap/contexts.rs` `ScriptContext::as_any` erased the lifetime to `'static`, so any crate could downcast and leak references | `as_any` returns a private `#[repr(transparent)] ErasedScriptContext` that only `from_input` can name (`cannot_downcast_to_public_static_context`) |
| **Unsound:** `collection/array_long_long_map_mmap.rs` check-then-act race between `read_map` and `new_writer` (a writer could write into an unmapped segment) | Writer registration and sealing happen under the same lock (`racing_read_and_new_writer_is_safe`, `writer_created_after_read_fails`) |
| **Risky:** same file, `MmapMut` `DerefMut` created `&mut [u8]` over memory other threads write | Segments are `MmapRaw`; no reference is ever created. `segment_bits` capped at 40 |
| **Risky:** `custommap/expression/stdlib.rs` blanket `unsafe impl<T> Send/Sync for Shared<T>` | `Shared` removed; `WithGeometry`/`SourceFeature` require `Send + Sync` |
| **Risky:** `collection/append_store_mmap.rs` unaligned typed read relying on a safe public `Primitive` trait | Decodes from a bounds-checked slice via `Primitive::read_ne`; `unsafe` removed |

### Current sites

All reviewed sites are believed sound. The table lists every remaining `unsafe` block in `planetiler-core` and
`planetiler-custommap`.

| Site | What it does | Verdict / notes |
|---|---|---|
| `collection/array_long_long_map_mmap.rs` (`Send for Writer`, atomic store, read) | Concurrent writes into `MmapRaw` segments; reads from the sealed read-only map | OK: aligned (page-aligned base, bounds-checked index), sealing synchronizes writers before reads |
| `collection/append_store_mmap.rs` `Mmap::map` | Read-only map of a private temp file after its writer is dropped | OK |
| `collection/external_merge_sort.rs` `Mmap::map` (2 sites) | Read sorted chunk files | OK: readers are dropped before chunks are truncated or removed |
| `collection/external_merge_sort.rs` `set_len` after parallel chunk reads | Exposes spare capacity filled by reader threads | **Added after the audit.** OK as long as every task fully initializes its disjoint range; `set_len` only runs when all tasks succeed |
| `reader/natural_earth_reader.rs` `sqlite3_config(MEMSTATUS, 0)` | Turn off sqlite memory statistics once per process | **Added after the audit.** Minor risk: `sqlite3_config` must not run concurrently with other sqlite use. It runs in a `Once` before Natural Earth opens connections; if another sqlite user were active on another thread at that moment it would be a data race inside sqlite. Today sources run sequentially and the mbtiles writer opens later |
| `reader/osm/osm_input_file.rs` `Mmap::map` | Lazy reads of the input `.osm.pbf` | OK (standard caveat: input must not be modified during the run; Java has the same with `MappedByteBuffer`) |
| `reader/osm/pbf_decoder.rs` `from_utf8_unchecked` | String table entries | OK: every entry is UTF-8-checked in `parse_block` first (invalid ones use a lossy cached copy). Fragile ordering; a `debug_assert!` would help |
| `util/parallel_inflate.rs` `literal`/`copy` | Hand-written inflate writing into reserved `Vec` capacity | **Removed** (2026-10-18). The reasoning held up (reservation before each step, `1 <= dist <= len` checked, strided copies only read initialized bytes, `set_len` after writing, pointers re-taken after any reallocation), but safe pushes cost only ~2% inflate CPU, so the module is now `#![forbid(unsafe_code)]`. Differential fuzzing against zlib-rs and libdeflate (`fuzz_parallel_inflate`, debug assertions + overflow checks: ~496K cases in 30 min on the unsafe version, then ~346K cases in 20 min on the safe one) found no wrong output or panic. It did find resource problems on crafted input, now fixed: huge blocks were decoded whole (GBs per worker), the block-start search was quadratic, and errors were not sticky. Miri and ASan need nightly and were not run; a cargo-fuzz target is in `fuzz/` |
| `util/zip_entry_reader.rs` `MmapOptions::map` | Map a zip entry's byte range | **Added after the audit.** OK (same caveat as other input mmaps) |
| `util/byte_buffer_util.rs` (madvise on a live `Mmap`, segment maps, test) | | OK; the public segment-mapping fn should document that callers must not modify the file while mapped |
| `util/madvise.rs` (`sysconf`, `posix_madvise` wrapper) | | OK: page-rounded range stays inside the mapping |
| `stats/process_info.rs` (`getrusage`, `gettid`, `pthread_mach_thread_np`, `sysconf`, `thread_info`, `clock_gettime`) | FFI for CPU/thread stats | OK |
| `custommap/contexts.rs` `as_any` | Lifetime-only cast to the private erased type | OK (see fix above) |

Outside the library crates: `planetiler-benchmarks/src/bin/benchmark_line_merge.rs` uses `static mut NUM_LINES`
(single-threaded, so fine, but an `AtomicUsize` would avoid the edition-2024 `static mut` lint).
`benchmark_simple_readers.rs` calls `getrusage` with a zeroed out-param (OK).

## Remaining `TODO(port)` markers (11)

Down from 53 at the time of the audit. Everything else was ported, swapped for the shared helper it was waiting on,
or relabeled as a documented difference from Java.

| Location | What's missing | Size | Impact |
|---|---|---|---|
| `geo/polygon_overlay.rs:431` | JTS snap rounding also adds hot pixels at segment intersections (and near-vertex tolerance); only vertices are hot pixels here | M | Medium: rare geometry/validity differences from Java in `reduce_precision` output |
| `geo/polygon_overlay.rs:917` | Snap rounding for lines (noding at intersections); lines are only rounded point by point | M | Low-medium: vertex-count and byte differences for merged/simplified lines |
| `collection/external_merge_sort.rs:815` | `ProgressLoggers` during the sort (percent of chunks, file size, pipeline stats) | S | Log cosmetics |
| `collection/feature_sort.rs:164` | Return a `Worker` instead of raw thread handles so pipeline stats can be logged | S | Log cosmetics |
| `stats/stats.rs:65` | `LogUtil.setStage` `[stage]` log-line prefix | S | Log cosmetics; makes Java/Rust log diffs noisier |
| `reader/structs.rs:16` | Cache parsed JSON for string-encoded struct values | S | Performance, only for JSON-in-string fields |
| `util/top_osm_tiles.rs:50`, `:116`; `planetiler/src/main.rs:108` | `top-osm-tiles` CLI: xz decompression of tile logs and `TileWeights::write_to_file` wiring | S | Low |
| `planetiler/src/main.rs:77` | `benchmark-mapping` (`OpenMapTilesMapping` benchmark) | S | Low |
| `tests/planetiler_runner_tests.rs:66` | In-memory stream archive readers for the csv/tsv/json/proto variants of `testPlanetilerRunner` | S-M | Test coverage only (writers are ported) |

Highest value next: snap rounding at intersections (parity). Parquet load balancing and general CRS support are done
(see PORTING.md "Simple readers" and "Coordinate reference systems").
