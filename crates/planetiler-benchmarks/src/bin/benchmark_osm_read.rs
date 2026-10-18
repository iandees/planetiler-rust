//! Port of `planetiler-benchmarks/.../BenchmarkOsmRead.java`.
//!
//! Repeatedly runs the equivalent of `OsmReader.pass1` with a null profile and no-op node location / multipolygon
//! storage, printing the elapsed time of each pass to stderr in the same format as Java
//! (`<wall> cpu:<cpu> avg:<cpus>`).
//!
//! Arguments (same names as Java, `--key=value` or `key=value`, `-`/`_` interchangeable):
//! - `osm_path` (default `data/sources/northeast.osm.pbf`) path to the `.osm.pbf` file (may also be passed as a bare
//!   positional argument)
//! - `threads` (default: number of CPUs, minimum 2)
//! - `osm_lazy_reads` (default `true`) read OSM blocks from disk in worker threads
//! - `iterations` (Rust-only, default: run forever like Java) number of passes to run
//! - `precise` (Rust-only, default `false`) append the exact wall/cpu milliseconds to each line, useful for small
//!   inputs like monaco where the Java format rounds to `0s`
//!
//! TODO(port): switch to `OsmReader::pass1` once it is ported (wave 2). Until then this replicates its per-element
//! work: parallel block decoding on `threads < 8 ? threads : threads - 1` workers (the noop node map supports parallel
//! writes), a nodes -> ways -> relations phaser, computing each node's encoded location, tracking the max way id, and
//! collecting way members of multipolygon relations.

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use planetiler_core::reader::osm::osm_block_source::{Block, OsmBlockSource};
use planetiler_core::reader::osm::osm_element::{OsmElement, OsmElementType, Relation};
use planetiler_core::reader::osm::osm_input_file::OsmInputFile;
use planetiler_core::reader::with_tags::WithTags;

fn parse_args() -> HashMap<String, String> {
    let mut result = HashMap::new();
    for arg in std::env::args().skip(1) {
        let arg = arg.trim_start_matches('-');
        match arg.split_once('=') {
            Some((k, v)) => result.insert(k.replace('-', "_"), v.to_string()),
            None if arg.ends_with(".pbf") => result.insert("osm_path".to_string(), arg.to_string()),
            None => result.insert(arg.replace('-', "_"), "true".to_string()),
        };
    }
    result
}

/// Minimal version of `OsmPhaser`: workers move through nodes -> ways -> relations, and a worker entering the ways
/// phase waits until all workers have finished processing nodes.
struct Phaser {
    workers: usize,
    // finished[p] = number of workers that have finished phase p (1 = nodes, 2 = ways, 3 = relations)
    finished: Mutex<[usize; 4]>,
    cv: Condvar,
    nodes: AtomicUsize,
    ways: AtomicUsize,
    relations: AtomicUsize,
}

const NODES: usize = 1;
const WAYS: usize = 2;
const RELATIONS: usize = 3;
const DONE: usize = 4;

impl Phaser {
    fn new(workers: usize) -> Phaser {
        Phaser {
            workers,
            finished: Mutex::new([0; 4]),
            cv: Condvar::new(),
            nodes: AtomicUsize::new(0),
            ways: AtomicUsize::new(0),
            relations: AtomicUsize::new(0),
        }
    }

    fn advance(&self, current: &mut usize, target: usize, wait: bool) {
        if *current < target {
            let mut finished = self.finished.lock().unwrap();
            for phase in (*current).max(1)..target {
                finished[phase] += 1;
            }
            *current = target;
            self.cv.notify_all();
            if wait && target >= 2 {
                let _guard = self.cv.wait_while(finished, |f| f[target - 1] < self.workers).unwrap();
            }
        }
    }
}

fn is_multipolygon(relation: &Relation) -> bool {
    relation.has_tag_any("type", &["multipolygon", "boundary", "land_area"])
        && relation.members.iter().any(|m| m.member_type == OsmElementType::Way)
}

struct Shared {
    phaser: Phaser,
    max_way_id: AtomicI64,
    ways_in_multipolygon: Mutex<HashSet<i64>>,
    blocks: AtomicUsize,
}

fn process_blocks(shared: &Shared, rx: crossbeam_channel::Receiver<Block>) -> anyhow::Result<()> {
    let phaser = &shared.phaser;
    let mut phase = 0;
    let mut max_way_id = i64::MIN;
    let (mut nodes, mut ways, mut relations) = (0, 0, 0);
    let mut sink = 0u64;
    for block in rx {
        for element in block.into_elements()? {
            if element.id() < 0 {
                anyhow::bail!("Negative OSM element IDs not supported: {element:?}");
            }
            match element {
                OsmElement::Node(node) => {
                    phaser.advance(&mut phase, NODES, true);
                    // noop node location writer: just compute the encoded location
                    sink = sink.wrapping_add(std::hint::black_box(node.encoded_location()));
                    nodes += 1;
                }
                OsmElement::Way(way) => {
                    phaser.advance(&mut phase, WAYS, true);
                    max_way_id = max_way_id.max(way.id);
                    ways += 1;
                }
                OsmElement::Relation(relation) => {
                    phaser.advance(&mut phase, RELATIONS, false);
                    if is_multipolygon(&relation) {
                        let mut set = shared.ways_in_multipolygon.lock().unwrap();
                        for member in &relation.members {
                            if member.member_type == OsmElementType::Way {
                                set.insert(member.ref_id);
                            }
                        }
                    }
                    relations += 1;
                }
                OsmElement::Other(_) => {}
            }
        }
        shared.blocks.fetch_add(1, Ordering::Relaxed);
    }
    phaser.advance(&mut phase, DONE, false);
    std::hint::black_box(sink);
    phaser.nodes.fetch_add(nodes, Ordering::Relaxed);
    phaser.ways.fetch_add(ways, Ordering::Relaxed);
    phaser.relations.fetch_add(relations, Ordering::Relaxed);
    shared.max_way_id.fetch_max(max_way_id, Ordering::Relaxed);
    Ok(())
}

fn pass1(file: &OsmInputFile, threads: usize) -> anyhow::Result<Shared> {
    let parse_threads = 1.max(if threads < 8 { threads } else { threads - 1 });
    let shared = Shared {
        phaser: Phaser::new(parse_threads),
        max_way_id: AtomicI64::new(i64::MIN),
        ways_in_multipolygon: Mutex::new(HashSet::new()),
        blocks: AtomicUsize::new(0),
    };
    let source = file.get()?;
    let (tx, rx) = crossbeam_channel::bounded::<Block>(parse_threads * 2);
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let workers: Vec<_> = (0..parse_threads)
            .map(|_| {
                let rx = rx.clone();
                let shared = &shared;
                scope.spawn(move || process_blocks(shared, rx))
            })
            .collect();
        drop(rx);
        let read_result = source.for_each_block(&mut |block| Ok(tx.send(block)?));
        drop(tx);
        for worker in workers {
            worker.join().expect("worker panicked")?;
        }
        read_result
    })?;
    Ok(shared)
}

/// Process CPU time (user + system) of this process.
fn cpu_time() -> Option<Duration> {
    // SAFETY: getrusage only writes into the provided struct
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        let to_duration = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
        Some(to_duration(usage.ru_utime) + to_duration(usage.ru_stime))
    }
}

/// Java `Format.decimal`: at most 1 fraction digit, trailing zeros dropped.
fn decimal(value: f64) -> String {
    let s = format!("{value:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// Java `Format.duration`: `0.5s`, `3s`, `1m1s`, `1h`, ...
fn format_duration(d: Duration) -> String {
    let seconds = d.as_secs_f64();
    if seconds < 1.0 {
        return format!("{}s", decimal(seconds));
    }
    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 {
        out.push_str(&format!("{s}s"));
    }
    out
}

fn main() -> anyhow::Result<()> {
    let args = parse_args();
    let path = args.get("osm_path").cloned().unwrap_or_else(|| "data/sources/northeast.osm.pbf".to_string());
    let threads: usize = args.get("threads").map(|t| t.parse()).transpose()?.unwrap_or_else(num_threads).max(2);
    let lazy = args.get("osm_lazy_reads").is_none_or(|v| v == "true");
    let iterations: Option<usize> = args.get("iterations").map(|t| t.parse()).transpose()?;
    let precise = args.get("precise").is_some_and(|v| v == "true");
    eprintln!("osm_path={path} threads={threads} osm_lazy_reads={lazy}");
    let file = OsmInputFile::new(&path, lazy);

    let mut i = 0;
    while iterations.is_none_or(|n| i < n) {
        let start = Instant::now();
        let start_cpu = cpu_time();
        let result = pass1(&file, threads)?;
        let wall = start.elapsed();
        let cpu = start_cpu.zip(cpu_time()).map(|(a, b)| b.saturating_sub(a));
        if i == 0 {
            eprintln!(
                "nodes={} ways={} rels={} blocks={} multipolygon_ways={}",
                result.phaser.nodes.load(Ordering::Relaxed),
                result.phaser.ways.load(Ordering::Relaxed),
                result.phaser.relations.load(Ordering::Relaxed),
                result.blocks.load(Ordering::Relaxed),
                result.ways_in_multipolygon.lock().unwrap().len()
            );
        }
        let cpu_string = cpu.map_or("-".to_string(), format_duration);
        let avg = cpu.map_or(String::new(), |c| format!(" avg:{}", decimal(c.as_secs_f64() / wall.as_secs_f64())));
        if precise {
            let cpu_ms = cpu.map_or(0.0, |c| c.as_secs_f64() * 1000.0);
            eprintln!(
                "{} cpu:{}{} (wall_ms={:.1} cpu_ms={:.1})",
                format_duration(wall),
                cpu_string,
                avg,
                wall.as_secs_f64() * 1000.0,
                cpu_ms
            );
        } else {
            eprintln!("{} cpu:{}{}", format_duration(wall), cpu_string, avg);
        }
        i += 1;
    }
    Ok(())
}

fn num_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get())
}
