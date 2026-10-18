# Review: `unsafe` audit and `TODO(port)` triage

Read-only review of `main` @ `c116961` (2026-10-17). Nothing was compiled or run. Line numbers are as of that commit.

**Status (2026-10-18):** all 2 unsound and 3 risky items in Part 1 are fixed (each section below has a **Fixed** note).
In Part 2, all 13 (a1) items, 12 of the 16 (a2) markers, #32 and all 8 (c) items are done; see
[Part 2 status](#part-2-status-2026-10-18). 20 `TODO(port)` markers remain.

## Part 1: `unsafe` audit

Scope: `crates/planetiler-core/src` (23 sites) and `crates/planetiler-custommap/src` (3 sites). The benchmarks crate is
covered briefly at the end.

### Summary

| Verdict | Count | Sites |
|---|---|---|
| **Unsound** (safe code can cause UB) | 2 | `custommap/contexts.rs:681` (lifetime-erasing `as_any`), `core/collection/array_long_long_map_mmap.rs` (seal race: `read_map` vs `new_writer`) |
| **Risky** (sound in today's callers but UB under the Rust aliasing model, or the invariant is too broad) | 3 | `array_long_long_map_mmap.rs:104/108` (`&mut [u8]` reborrow over memory other threads are writing), `custommap/expression/stdlib.rs:55,57` (blanket `Send`/`Sync` for `Shared<T>`), `append_store_mmap.rs:112` (depends on a safe public `Primitive` trait) |
| OK | 21 | the rest (FFI calls, read-only mmaps of private temp files, UTF-8 check that really does happen first) |

### Unsound

#### U1. `crates/planetiler-custommap/src/contexts.rs:681`: `ScriptContext::as_any` transmutes `&ScriptContext<'a>` to `&ScriptContext<'static>`

**Fixed:** `as_any` now hands out a private `#[repr(transparent)] struct ErasedScriptContext(ScriptContext<'static>)`
that only `from_input` can name; downcasting to `ScriptContext<'static>` returns `None` (tested in
`contexts::tests::cannot_downcast_to_public_static_context`).

The SAFETY comment says "the only way to get the context back out is `ScriptContext::from_input`". That isn't true.
`ExpressionInput::as_any` is a public trait method and `ScriptContext` is a public type, so any crate can downcast to
`ScriptContext<'static>` directly. Its public accessors then return `'a`-typed references, which come out as `'static`:

```rust
// compiles in safe code in any downstream crate:
let ctx: &ScriptContext<'static> = input.as_any()?.downcast_ref::<ScriptContext<'static>>()?;
let leaked: Option<&'static Value> = ctx.value();            // also root(), feature(), match_keys(), tag_value_producer(), argument()
// `leaked` outlives the feature/value it points into -> use-after-free
```

`root() -> &'a Root`, `feature() -> Option<FeatureRef<'a>>`, `value()`, `argument()`, `match_key()`,
`tag_value_producer()` and `match_keys()` all leak this way. No caller does this today, but safe code can, so the
code is unsound.

**Fix (smallest change):** downcast to a private type that no outside code can name:

```rust
// contexts.rs (private, not pub(crate))
#[repr(transparent)]
struct ErasedScriptContext(ScriptContext<'static>);

fn as_any(&self) -> Option<&dyn Any> {
    // SAFETY: lifetime-only transmute; `ErasedScriptContext` is private to this module, so the only code that can
    // downcast to it is `from_input`, which shortens the lifetime back to the borrow of `input`.
    let erased: &ErasedScriptContext = unsafe { &*(self as *const ScriptContext<'_> as *const ErasedScriptContext) };
    Some(erased)
}
pub fn from_input<'b>(input: &'b dyn ExpressionInput) -> Option<&'b ScriptContext<'b>> {
    input.as_any()?.downcast_ref::<ErasedScriptContext>().map(|e| shorten(&e.0))
}
```

A cleaner alternative is to drop `Any` here and add `fn script_context(&self) -> Option<&dyn ScriptContextLike>` to
the trait, or to use a crate like `better_any`/`transient` that does lifetime-aware downcasting.

#### U2. `crates/planetiler-core/src/collection/array_long_long_map_mmap.rs`: TOCTOU between `read_map` and `new_writer`/`segment_ptr`

**Fixed:** `new_writer` increments `active_writers` under the write lock, and `read_map` takes that lock before
checking the count and sealing. Writers created after sealing fail on their first `put`. Tests:
`writer_created_after_read_fails`, `read_with_open_writer_panics`, `racing_read_and_new_writer_is_safe`.

`read_map` (lines 113-126) does `assert_eq!(0, active_writers)` *before* it takes the `write` lock. It then sets
`sealed` and drops every writable `MmapMut`. `new_writer` (line 165) increments `active_writers` without taking the
lock or checking `sealed`. This interleaving is possible using only safe methods:

1. Thread B, in `get` -> `read_map`, sees `active_writers == 0`.
2. Thread A calls `new_writer()` then `put()`. `segment_ptr` takes the lock, sees `sealed == false`, maps the segment,
   returns `base`, and releases the lock.
3. Thread B takes the lock, sets `sealed`, and runs `segments.clear()`, which **unmaps** the segment.
4. Thread A's next `put` writes through `base` into unmapped memory: SIGSEGV, or a write into whatever gets mapped
   at that address later.

A writer that is created after `sealed` is set is fine, because `base` is null, so it goes through `segment_ptr` and
bails. Planetiler only reads after pass 1 has finished, so this never happens in practice. But the `LongLongMap`
API is safe, so the code is unsound.

**Fix:** check and change the writer count under the same lock that sealing uses:

```rust
fn new_writer(&self) -> Box<dyn LongLongMapWriter + '_> {
    let state = self.write.lock();
    assert!(!self.sealed.load(Acquire), "cannot write after reading");   // or return a failing writer
    self.active_writers.fetch_add(1, AcqRel);
    drop(state);
    ...
}
// read_map: take `self.write.lock()` FIRST, then assert active_writers == 0, then seal + clear.
```

### Risky

#### R1. `array_long_long_map_mmap.rs:104,108`: `MmapMut::as_mut_ptr` goes through `DerefMut`, creating a `&mut [u8]` that aliases memory other threads are writing

**Fixed:** segments are mapped with the safe `MmapOptions::map_raw` and stored as `MmapRaw`, whose
`as_mut_ptr(&self)` never creates a reference (this also removed the `unsafe` around mapping). `segment_bits` is now
capped at 40 (`rejects_huge_segments`).

`memmap2 0.9.11` has no inherent `MmapMut::as_mut_ptr`. Only `MmapRaw` has one. So
`state.segments[idx].as_mut().unwrap().as_mut_ptr()` auto-derefs to `<[u8]>::as_mut_ptr(&mut [u8])`. Every time
*any* writer enters a segment (line 108 runs even when the segment is already mapped), it creates a fresh, unique
`&mut [u8]` over the whole 1GB segment. Meanwhile other threads are doing atomic stores through raw pointers taken
from earlier `&mut` borrows. Under Stacked Borrows this is UB in two ways: the retag conflicts with concurrent
accesses, and it invalidates the other writers' pointer tags. Tree Borrows is more lenient but still flags the
conflict. It doesn't miscompile today because the pointer crosses a mutex and is opaque to the optimizer, but it is
the kind of thing Miri would report.

**Fix:** map with `MmapOptions::map_raw(file)` and store `MmapRaw`, whose `as_mut_ptr(&self)` never creates a
reference. Or store the base `*mut i64` once, at map time, next to the `MmapMut` and return that.

The rest of this file is OK:
- The `Send for Writer` reasoning holds, given that `ArrayLongLongMapMmap` is auto-`Sync`.
- `AtomicI64` stores are 8-byte aligned (page-aligned base, `offset < 2^(bits-3)`).
- Read-after-write visibility comes from the `AcqRel` decrement in `Writer::drop` and the `Acquire` load in `read_map`.
- The file is never truncated while mapped. `set_len` only grows it, and `Drop` unmaps before `remove_file`.
- Moving `MmapMut`s when the `Vec` resizes doesn't move the mapped pages.

Minor: `with_segment_bits` accepts any `segment_bits >= 3`. A value of 64 or more would overflow
`1u64 << segment_bits` and `index << segment_bits`. Clamp it to something like 40.

#### R2. `crates/planetiler-custommap/src/expression/stdlib.rs:55,57`: `unsafe impl<T> Send/Sync for Shared<T>`

**Fixed:** `Shared` is gone. `WithGeometry` (and so `SourceFeature`) now has `Send + Sync` supertraits, which every
implementation already satisfied, so `GeometryVal`/`Resolver` are auto `Send + Sync`
(`stdlib::tests::values_borrowing_features_are_send_sync`).

The impl is a blanket one for **every** `T`. It is only `pub(crate)`, and today it wraps `&'a dyn WithGeometry`
(lines 330/344/563) and `ScriptContext<'a>` (line 512). The SAFETY argument is "only used on the creating thread
during one CEL evaluation". That holds today. But it depends on the `cel` crate (0.15) never moving values across
threads (rayon, a shared program cache) and on nobody in the crate wrapping an `Rc`/`RefCell`. The compiler can't
check either.

Every concrete `SourceFeature` already uses `OnceLock` (`SimpleFeature`, `ParquetFeature`, `OsmSourceFeature`), so
they are all `Sync` anyway.

**Fix:** remove `Shared`. Add `Send + Sync` supertraits to `WithGeometry`/`SourceFeature` (or use
`&'a (dyn WithGeometry + Sync)` and `&'a (dyn SourceFeature + Sync)` in `FeatureRef`). Then `GeometryVal` and
`Resolver` are auto-`Send + Sync` and no `unsafe` is needed. If that's too invasive, at least restrict the impls to
those two concrete payloads instead of `impl<T>`.

#### R3. `crates/planetiler-core/src/collection/append_store_mmap.rs:112`: `(map.as_ptr() as *const T).add(i).read_unaligned()`

**Fixed:** `Primitive` gained `fn read_ne(&[u8]) -> Self`, and `get` decodes `&map[i * BYTES..(i + 1) * BYTES]`, so the
`unsafe` is gone and a bad impl can only panic or misread (`append_store_mmap::tests::decodes_with_encoded_width`).

This is sound for the two impls that exist (`i32`, `i64`). However, `Primitive` (`append_store_ram.rs:8`) is a *safe*
public trait in a public module, and the read relies on two things it doesn't enforce:
1. Every bit pattern is a valid `T`.
2. `T::BYTES == size_of::<T>()`. The file is written with `T::BYTES` per element but indexed with `size_of::<T>()`.

A downstream `impl Primitive for bool` or a mismatched `BYTES` gets UB or out-of-bounds reads from safe code. Only
the `debug_assert!` on line 110 catches this.

**Fix:** make `Primitive` sealed (`pub trait Primitive: sealed::Sealed`) or an `unsafe trait`. Alternatively add
`fn read_ne(&[u8]) -> Self` and decode from the bounds-checked slice
`&map[i*T::BYTES..(i+1)*T::BYTES]`, which removes the `unsafe` and costs roughly nothing.

### OK

| Site | What it does | Notes |
|---|---|---|
| `reader/osm/pbf_decoder.rs:527` `from_utf8_unchecked` | String table entries from a PBF block | `parse_block` (lines 437-443) runs `from_utf8` on every entry and stores a lossy copy in `cache` for any that fail. `Strings::str` checks the cache first. The invariant holds because `Strings` is only built after `parse_block` succeeds; on failure `decode` returns the buffers without making a `PbfElements`. It's fragile (it depends on that ordering), so consider adding a `debug_assert!(std::str::from_utf8(bytes).is_ok())`. Slicing is bounds-checked. |
| `reader/osm/osm_input_file.rs:88` `Mmap::map` of the input `.osm.pbf` | Lazy block reads | The usual mmap caveat applies: if the user truncates or modifies the input during a run you get SIGBUS or torn reads. That's accepted practice (Java's `MappedByteBuffer` has the same issue). Every access is a bounds-checked slice. |
| `collection/append_store_mmap.rs:28` `map_read_only` | Read-only map of a private temp file after its writer has been flushed and dropped | The writer is `take()`n before mapping, and `append` bails after that. Also used by `array_long_long_map_mmap.rs:123` after every writable map has been dropped. |
| `collection/external_merge_sort.rs:464` `Mmap::map(chunk)` | Read a sorted chunk | `sort_group` drops the reader (scoped in the `for` loop) before `open_writer` truncates `group[0].path`, and before `remove_file` on the other chunks. `mmap && compress` is forced to `mmap = false` (line 130). Reads are bounds-checked (`bytes.get(..)`). In `reuse_feature_db` mode another process could rewrite the chunk dir; that's the same accepted caveat as above. |
| `util/byte_buffer_util.rs:51` | `madvise` over the exact range of a live `Mmap` | Correct. |
| `util/byte_buffer_util.rs:91` `MmapOptions::offset().len().map(file)` | Map segments of a caller-supplied file | A safe `pub fn` that maps an arbitrary `&File`: same caveat. memmap2 aligns the offset itself. Acceptable, but worth a doc note that the caller must not modify the file while it's mapped. |
| `util/byte_buffer_util.rs:123` | Test only | n/a |
| `util/madvise.rs:16` `sysconf` | No preconditions | OK |
| `util/madvise.rs:41` `pub unsafe fn posix_madvise` | Contract: the range is inside one live mapping | Rounding the range out to page boundaries stays inside the pages memmap2 actually mapped. With `POSIX_MADV_DONTNEED` on a file-backed shared map, pages are dropped, not zeroed, so data is safe. |
| `util/madvise.rs:53` | libc call | OK |
| `stats/process_info.rs:24-25` `getrusage` | Zeroed POD out-param | OK |
| `stats/process_info.rs:163` `gettid` | No preconditions | A reused tid gives wrong stats, not UB. |
| `stats/process_info.rs:166` `pthread_mach_thread_np(pthread_self())` | Port name for the current thread | Takes no send-right reference, so a stale port after the thread exits just makes `thread_info` fail. `Registration::drop` removes the entry anyway. |
| `stats/process_info.rs:181` `sysconf(_SC_CLK_TCK)` | | OK |
| `stats/process_info.rs:189` `thread_info` | `count = THREAD_BASIC_INFO_COUNT` matches `thread_basic_info` | OK |
| `stats/process_info.rs:292-293` `clock_gettime` | | OK |

### Out of scope, noted

`crates/planetiler-benchmarks/src/bin/benchmark_line_merge.rs:15-20,246` uses `static mut NUM_LINES` with
`unsafe { NUM_LINES += n }`. It is single-threaded, so it's OK, but `static mut` references are deny-by-default in
edition 2024. Use an `AtomicUsize` or a `Cell` in a thread-local instead. `benchmark_geometry_cpu.rs`
(a `GlobalAlloc` that just forwards to `System`) and `benchmark_osm_read.rs:182` (`getrusage`) are OK.

---

## Part 2: `TODO(port)` triage

There are **53** `TODO(port)` markers: 47 in `src/` and 6 in `tests/`. They're grouped as follows:

- **(a1) Stale, mechanical**: the target exists and the change is a drop-in replacement. **13**
- **(a2) Stale, unblocked**: the dependency the TODO waits on now exists, but some new code is needed. **16**
- **(b) Real missing feature.** **16**
- **(c) Intentional difference or inherited Java TODO**: document it, then delete the `TODO(port)` (or turn it into a plain `TODO` if Java has the same one). **8**

So 29 are stale (a1 + a2), 16 are real gaps and 8 are intentional.

### Part 2 status (2026-10-18)

| Rows | Status |
|---|---|
| #1-#13 (a1) | **Done** as described, with these details: #2 points both pmtiles helpers at `util::gzip` (`gzip_zlib`, and `util::gzip::gunzip` now uses `MultiGzDecoder`); #5 keeps thin `Geometry`-returning wrappers over `geo::test_utils`; #6 adds `imposm3_parsers::bool_str`; #12 also moved `FieldType::of` into the archive type. |
| #14 custommap `SchemaValidator` | **Done.** `planetiler_custommap::validator::{validate, SchemaValidator, main}` on top of `BaseSchemaValidator` (embedded or referenced examples, `--watch`, `verify schema.yml` shortcut, "Malformed expression"/"Malformed yaml input" messages). `planetiler verify`/`verify-custom`/`verify-schema` run it. `tests/schema_tests.rs` uses the core validator (the ~300-line reimplementation is gone) and checks the CLI output like Java's `SchemaValidatorTest`. |
| #15 `benchmark_osm_read` | **Done.** Runs `OsmReader::pass1` with `NullProfile` and noop node/multipolygon storage. |
| #16 parquet `Distributor` | Not done: a performance change (S-M) that needs measuring on Overture inputs; out of scope for this cleanup. |
| #17 `feature_sort` `Worker`, #18 `external_merge_sort` `ProgressLoggers` | Not done: the sort code is being optimized concurrently; left alone to avoid merge conflicts. |
| #19 `top_osm_tiles` `write_to_file` | Not done: the CLI is still blocked on xz (#33). |
| #20 `ArchiveOptions` | **Done.** Removed; every archive takes `&Arguments` (`apply_fallbacks` is `Arguments::of_map(query).or_else(args.with_prefix(format))`). |
| #21 `CompareArchives` geometry | **Done**, plus a behavior change: layers are paired by name, feature tags are compared as decoded key/value pairs, geometries with different commands are decoded (Java's `compareGeometry` checks) and are equal if they decode the same, and tiles that only differ in encoding are counted in `CompareResult::equivalent_tiles` instead of `tile_diffs`. |
| #22 `mbtiles::compare` | **Done.** Compares `to_wkt(norm(decode()))` like Java. |
| #23 `Verify` feature counts | **Done.** `Verify::{get_num_features, check_min_feature_count, check_min_feature_count_range}`; `verify_monaco` uses them. |
| #24, #25 `Verify` validity | **Done.** `check_validity` decodes every feature and requires polygons to be simple; `is_simple_polygonal` moved to `geo::is_valid`; `test_invalid_geometry` added. |
| #26 `Stats` debug log | **Done** (WKB logged as hex, no base64 dependency). |
| #32 free-after-read disk estimate | **Done.** `ResourceUsage::free_disk` (usage amounts are now signed internally). |
| #38-#45 (c) | **Done.** Relabeled as "Difference from Java" notes, plain `TODO`s where Java has the same TODO, or a short comment (#38, #42/#43). PORTING.md no longer lists "unify VectorLayer" as a follow-up. |
| #27-#31, #33-#37 (b) | Unchanged (real missing features). |

### (a1) Stale, mechanical (13)

| # | Location | Replacement |
|---|---|---|
| 1 | `core/src/pmtiles/pmtiles.rs:244` var_int | Delete the local `put_var_long`/`get_var_long`. Encode with `crate::util::var_int::put_var_long(v as i64, out)`. Decode with `let mut s = &bytes[*pos..]; let v = var_int::try_get_var_long(&mut s).ok_or_else(\|\| FileFormatException("Unexpected end of PMTiles directory".into()))? as u64; *pos = bytes.len() - s.len();`. The util version caps at 10 bytes instead of returning "Varint too long", which is fine. |
| 2 | `core/src/pmtiles/pmtiles.rs:363` gzip | `gzip` -> `crate::util::gzip::gzip_zlib` (byte-identical to the current flate2 output and to Java). Keep `pmtiles::gunzip` (it uses `MultiGzDecoder`, which matches Java's `GZIPInputStream` on multi-member streams), or switch `util::gzip::gunzip` to `MultiGzDecoder` and point both at it. |
| 3 | `core/src/vector_tile.rs:95` `empty_geometry` | `use crate::geo::geo_utils::empty_geometry;` and delete the local fn. |
| 4 | `core/src/vector_tile.rs:1252` `WithTags` for `Feature` | `impl crate::reader::with_tags::WithTags for Feature { fn tags(&self) -> &Tags { &self.tags } }` |
| 5 | `core/src/vector_tile.rs:1643` test helpers | Use `crate::geo::test_utils::{new_coordinate_list, new_point, new_multi_point, new_line_string, new_multi_line_string, new_polygon, new_multi_polygon}`. They return concrete geo types (`Point`, `Polygon`, ...), so add `.into()` / `Geometry::from(..)` at call sites, or keep thin `Geometry` wrappers. |
| 6 | `core/src/reader/with_tags.rs:166` `parse_shim` | Replace with `crate::util::parse` / `imposm3_parsers`: `bool(tag)` -> `tag.is_some_and(parse::bool)`, `direction` -> `tag.map_or(0, parse::direction)`, `parse_long` -> `tag.map_or(0, parse::parse_long)`, `parse_*_or_null_value` -> `tag.and_then(parse::parse_*_or_null)`, string forms -> `parse::parse_*_or_null_str`, `bool_str(s)` -> `imposm3_parsers::bool(&Value::str(s))` (or add a `bool_str`), `wayzorder` -> `parse::wayzorder`. **Caveat:** the two lenient number parsers differ slightly. The shim requires a digit before `,`; util accepts a leading `,`. util also uses `java_parse_double` first. Run the `with_tags`/`structs`/`data_type` tests after switching. Users: `with_tags.rs:84-100`, `structs.rs:190-221`, `expression/data_type.rs:76-80`. |
| 7 | `core/src/collection/long_long_map.rs:342` | Port the Java assertion inside the loop: `let est = OsmReader::estimate_node_location_usage(map_type, storage, 70_000_000_000, &params.path); let usage = if storage == Storage::Mmap { est.disk_usage() } else { est.get(&[&resource_usage::MEMORY /* or DIRECT */]) }; if map_type != Noop { assert!(usage > 60e9 as u64 && usage < 100e9 as u64, "{variant}") }`. Check whether `reader::osm` can be imported from `collection` tests without a cycle; if not, put the test in `osm_reader.rs`. |
| 8 | `core/src/collection/external_merge_sort.rs:278` `bin_pack` | `crate::util::bin_pack::pack(chunks, max_bin_size as i64, \|c\| c.bytes_in_memory as i64)`. Note the tie-breaking: the local `max_by_key` picks the *last* equally-full bin, while util/Java picks the *first*. Switching gives Java's grouping. Sorted output doesn't change. |
| 9 | `core/src/util/downloader.rs:534` | `ResourceUsage::new("download").add_disk(Some(path), size, &format!("download {id}")).check_against_limits(force, false)`, then delete the `sysinfo` disk lookup. |
| 10 | `core/src/util/tileset_summary_statistics.rs:52` | `format_storage(n, pad)` -> `crate::util::format::storage(n, pad)`, `format_integer` -> `crate::util::format::integer`. Check that the `<1` / `-` edge cases match. |
| 11 | `core/src/util/tileset_summary_statistics.rs:90` | `red`/`yellow` -> `crate::util::ansi_colors::{red, yellow}`. As a bonus, this respects `set_use_colors`. |
| 12 | `core/src/archive/tile_archive_metadata.rs:62` `FieldType` | The two enums are identical. Add `pub use crate::archive::tile_archive_metadata::FieldType;` in `util/layer_attr_stats.rs` and delete its copy and the `From<FieldType>` impl. |
| 13 | `core/src/config/arguments.rs:852` | `assert_eq!(Envelope::new(7.409205, 7.448637, 43.72335, 43.75169), *Bounds::new(Arguments::of().bounds("bounds","bounds").unwrap()).add_fallback_provider(&OsmInputFile::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/monaco-latest.osm.pbf"), false)).lat_lon());` (adjust the deref and borrow to the actual `add_fallback_provider` signature). |

### (a2) Stale, unblocked: the dependency exists now (16)

| # | Location | What's needed | Size |
|---|---|---|---|
| 14 | `planetiler/src/main.rs:46` + `custommap/src/validator.rs:3` + `custommap/tests/schema_tests.rs:3` (3 TODOs) | `planetiler_core::validator::{base_schema_validator, schema_specification}` now exist. Implement `SchemaValidator: BaseSchemaValidator` in `custommap/validator.rs` (`validate(schema, spec)` using `build_root_context` + `ConfiguredProfile` + `base_schema_validator::validate`; `main` handling `--watch` and the `verify schema.yml` shortcut). Delete the ~400-line test-only reimplementation in `schema_tests.rs`. Wire up `verify_schema` in the CLI. `java_profile_validator.rs` (132 lines) is the model to follow. | S-M (~150 LOC net negative) |
| 15 | `benchmarks/src/bin/benchmark_osm_read.rs:16` | `OsmReader::pass1` exists (`osm_reader.rs:560`). Build an `OsmReader` with a noop node map and a test profile, like Java's `BenchmarkOsmRead`, and call `pass1(&config)` each iteration. Delete the hand-copied pass-1 loop. | S |
| 16 | `core/src/reader/parquet/parquet_reader.rs:12` | `worker::distributor::Distributor` exists. Wrap each block's per-feature work in `distributor.for_thread(..)` the way Java does, so idle threads help with large row groups. **This matters for performance on Overture inputs**, where some row groups are much larger than others. | S-M |
| 17 | `core/src/collection/feature_sort.rs:164` | Return or hold a `worker::Worker` (`Worker::new(prefix, stats, threads, task)`) instead of raw `JoinHandle`s, so `ProgressLoggers::add_pipeline_stats`/`add_thread_pool_stats_for_worker` can report reader threads. | S |
| 18 | `core/src/collection/external_merge_sort.rs:601` | `ProgressLoggers::create().add_percent_counter("chunks", num_groups, \|\| done).add_file_size_path(dir).add_process_stats().add_thread_pool_stats(..)`, and call `.log()` in the wait loop instead of the ad-hoc `info!`. | S |
| 19 | `core/src/util/top_osm_tiles.rs:116` | `TileWeights::write_to_file` exists. Wire it into the `top-osm-tiles` CLI. Still blocked on xz (#33) for real downloads. | XS |
| 20 | `core/src/archive/tile_archive_config.rs:453` `ArchiveOptions` | `Arguments` now has `or_else`, `with_prefix`, `subset`, `of_pairs`, typed getters and `"a\|b"` alternative keys (arguments.rs:207). Replace `ArchiveOptions` with `Arguments`. Check that per-key description logging doesn't get noisier. | S-M |
| 21 | `core/src/util/compare_archives.rs:388` | `VectorGeometry::decode` exists (vector_tile.rs:1006). Port Java's `compareGeometry(Geometry, Geometry)` (type, number of geometries, exterior and interior rings per polygon) for when the command streams differ. | S (~40 LOC) |
| 22 | `core/src/mbtiles/compare.rs:6` | Decode with `vector_tile::decode` + `VectorGeometry::decode`, then compare with `geo::normalize::norm`/`equals_norm`, like Java. At the moment the comparison is stricter than Java's. | S |
| 23 | `core/src/mbtiles/verify.rs:82` + `openmaptiles/src/util/verify_monaco.rs:85` (2 TODOs) | Move `get_num_features`/`check_min_feature_count[_range]` from `verify_monaco.rs` into `mbtiles::verify` (already listed in PORTING follow-ups). | S |
| 24 | `core/src/mbtiles/verify.rs:130` | In `check_validity`, decode each feature (`decode(data)` -> `f.geometry.decode()`) and fail when a polygon isn't simple. `geo::test_utils::is_simple_polygonal` already implements JTS `isSimple` for polygons; move it into `geo` proper (for example `geo::is_valid`) since it's no longer test-only. | S |
| 25 | `core/src/mbtiles/verify.rs:202` | Add `testInvalidGeometry` once #24 is done. | XS |
| 26 | `core/src/stats/stats.rs:126` | `feature_group::get_current_tile_for_debugging()` and the `wkb` crate both exist. When the tile is `Some`, `info!("{code} on {tile} {other} for wkb: {}", base64(wkb::writer::write(input)))`. Needs `base64` (not a workspace dep yet), or log hex. | XS |

### (b) Real missing features (16)

| # | Location | Feature | Size | Priority |
|---|---|---|---|---|
| 27 | `core/src/vector_tile.rs:781`, `core/src/archive/tile_archive_writer.rs:109`, `core/src/util/tile_size_stats.rs:20`, `core/tests/planetiler_tests.rs:137`, `core/tests/planetiler_tests_a.rs:52` (5 TODOs) | **Done**: MLT (MapLibre Tile) output is a port of the Java MLT 0.0.10 encoder (`planetiler_core::mlt`) with a decoder for `computeMltTileStats`, tests and `compare`; output is byte-identical to Java (PORTING.md "MLT output notes"). | - | - |
| 28 | `openmaptiles/src/layers/boundary.rs:222` | **Done**: JTS `Polygonizer` is ported (`geo::polygonizer`) and `boundary` builds country polygons with it like Java (polygonize, then union). | - | - |
| 29 | `core/src/geo/polygon_overlay.rs:387` | JTS snap rounding: **hot pixels at segment intersections** (and near-vertex tolerance). Today only vertices are hot pixels and the integer overlay rounds the rest. | M | Medium-high: rare differences from Java in `reduce_precision` output for polygons. Could add slivers or validity differences after tile clipping. |
| 30 | `core/src/geo/polygon_overlay.rs:455` | **Snap-rounding for lines** (noding at intersections). Lines are only rounded point by point. | M | Low-medium: JTS nodes self-intersecting lines during `GeometryPrecisionReducer`, which changes vertex counts and output bytes for merged or simplified lines. |
| 31 | `core/src/geo/crs.rs:15` | Arbitrary EPSG codes and general WKT CRS. Only 4326/4269/3857, PROJ strings and simple WKT work. | M (`proj4rs` + `crs-definitions` feature + a WKT -> PROJ converter for `.prj` files) | Medium: shapefiles or geopackages in other projections fail where Java (GeoTools) succeeds. |
| 32 | `core/src/planetiler.rs:1136` | The write-phase disk estimate doesn't subtract inputs marked `free_<name>_after_read`. | XS (~10 LOC: for each `input_paths` entry with `free_after_reading`, `write_phase.add_disk(Some(&p.path), -(size))`. Needs a signed amount, or compute it as `saturating_sub`.) | Low-medium: can make `check_disk_space` fail falsely on tight disks unless `--force` is used. |
| 33 | `planetiler/src/main.rs:102`, `core/src/util/top_osm_tiles.rs:50` (2 TODOs) | xz decompression for openstreetmap.org tile logs (`top-osm-tiles` CLI). | S (add `xz2` or the pure-Rust `lzma-rs`; the default fetcher becomes `XzDecoder::new(http_get(url))`) | Low |
| 34 | `core/tests/planetiler_runner_tests.rs:64` | In-memory stream archive readers (Java `InMemoryStreamArchive`) for the csv/tsv/json/proto/pbf variants of `testPlanetilerRunner` (the mbtiles/pmtiles/files MLT variants are ported). | S-M | Low (test coverage only; the writers are already ported) |
| 35 | `core/src/stats/stats.rs:65` | `LogUtil.setStage`: `[stage]` prefix on log lines. | S (a `tracing` span per stage, or a thread-local prefix in the fmt layer) | Low (log cosmetics; makes Java and Rust log diffs noisier) |
| 36 | `core/src/reader/structs.rs:13` | Cache parsed JSON for string values in `Struct::get`. | S (a `OnceLock<Value>` alongside string values, or a side cache) | Low (performance, only for string-encoded JSON fields) |
| 37 | `planetiler/src/main.rs:71` | `benchmark-mapping` (`OpenMapTilesMapping` benchmark). | S | Low |

### (c) Intentional differences or inherited Java TODOs (8): remove `TODO(port)`

| # | Location | Reason / action |
|---|---|---|
| 38 | `core/src/vector_tile.rs:101` `is_ccw` | The exact JTS `Orientation.isCCW` port has a single caller. The other "isCCW" in `render/tiled_geometry.rs:1109` is a test-only signed-area variant with different semantics. Keep it here (or make it `pub` in `geo_utils` if a second caller appears) and delete the TODO. |
| 39 | `core/src/reader/structs.rs:12` | `Value` intentionally has no `Instant`/`LocalDate`/`LocalTime`/`UUID` variants. `parquet_primitive_converter.rs:14-19` already documents that they map to their Java `toString()` strings (which is what ends up in the tiles) and that `Struct::as_timestamp` parses them back. Change the comment to a "Difference from Java" note. |
| 40 | `core/src/validator/base_schema_validator.rs:39` | Java has the same TODO (`BaseSchemaValidator.java:265`). Change it to a plain `TODO`. |
| 41 | `core/src/validator/base_schema_validator.rs:271` | Java has the same TODO (`BaseSchemaValidator.java:182`). Change it to a plain `TODO`. |
| 42 | `core/src/archive/tile_archive_metadata.rs:82` `VectorLayer` | The two types differ on purpose. The stats one uses `IndexMap<String, FieldType>` (insertion order) and `u32` zooms; the archive one uses `BTreeMap` (sorted, as written to metadata) and `i32` zooms. Keep the `From` conversion. Remove this TODO and #43, and drop the "unify VectorLayer" follow-up from PORTING.md (or downgrade it to a nice-to-have). |
| 43 | `core/src/util/layer_attr_stats.rs:183` | Same as #42. |
| 44 | `core/tests/planetiler_osm_tests.rs:162` | Rust has no `Error`/`Exception` split: every profile panic is caught. It's already `#[ignore]`d with a reason. Replace the TODO with a "Difference from Java" comment. |
| 45 | `custommap/src/configured_feature.rs:459` | `merge_polygons.min_area*` is a TODO in Java too. Change it to a plain `TODO`. |

(The 53 markers map onto rows 1-45: some rows cover several markers. Rows 14 and 27 cover 3 and 5 markers, and rows 23 and 33 cover 2 each.)

### Top 5 real gaps (parity and performance)

1. ~~**JTS `Polygonizer` for OMT boundary `adm0_l`/`adm0_r`** (#28).~~ Done (`geo::polygonizer`).
2. **Snap rounding at segment intersections in `reduce_precision`** (#29, plus #30 for lines). M. It is a source of
   geometry and validity differences from Java in tile output, so it makes Java-vs-Rust archive comparisons noisy.
3. ~~**MLT encoder** (#27, five markers).~~ Done (`planetiler_core::mlt`, byte-identical to Java).
4. **Parquet `Distributor` load balancing** (#16). S-M, and the dependency is already ported. It is the best
   performance gain per line of code for Overture builds, where uneven row-group sizes leave threads idle at the
   end of the read phase.
5. **General EPSG/WKT CRS support** (#31). M. Inputs that Java accepts fail outright in Rust.

The best quick wins are #32 (free-after-read disk estimate, ~10 LOC) and #14 (the custommap `SchemaValidator`/
`verify-schema` CLI), which removes about 400 lines of duplicated test code.
