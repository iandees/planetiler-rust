//! Port of `reader/osm/OsmPhaser.java`: coordinates multiple workers processing OSM elements sequentially.
//!
//! - Ensures nodes are processed first, then ways, then relations
//! - Keeps a count of elements processed
//! - Lets workers wait to start an element type until all workers are finished with the previous element type
//!   ([`ForWorker::arrive_and_wait_for_others`])
//! - Logs when starting a new phase, including how long the previous phase took and the number of elements per second
//!
//! Each worker calls [`OsmPhaser::for_worker`] to get a handle for that worker to coordinate with others.
//!
//! Java's `java.util.concurrent.Phaser` is replaced by a small mutex + condvar phaser with the same semantics
//! (`bulkRegister`, `register`, `arrive`, `awaitAdvance`). Element counts are kept per worker in atomics that only
//! that worker writes (plain loads/stores, no read-modify-write per element) and summed on read.

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar, Mutex};

use crate::stats::timer::Timer;
use crate::util::format;

/// The element type a worker is currently processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Before processing any OSM elements.
    Begin = 0,
    Nodes = 1,
    Ways = 2,
    Relations = 3,
    /// After finished processing all OSM elements.
    Done = 4,
}

impl Phase {
    const ALL: [Phase; 5] = [Phase::Begin, Phase::Nodes, Phase::Ways, Phase::Relations, Phase::Done];

    fn from(number: i32) -> Phase {
        if number < 0 { Phase::Begin } else { Phase::ALL[(number as usize).min(4)] }
    }

    fn number(self) -> i32 {
        self as i32
    }

    pub fn next(self) -> Phase {
        Phase::from(self.number() + 1)
    }

    pub fn prev(self) -> Phase {
        Phase::from(self.number() - 1)
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Phase::Begin => "begin",
            Phase::Nodes => "nodes",
            Phase::Ways => "ways",
            Phase::Relations => "relations",
            Phase::Done => "done",
        })
    }
}

struct PhaserState {
    /// current phase number (Java `Phaser.getPhase()`)
    phase: i32,
    parties: i32,
    unarrived: i32,
    /// number of [`ForWorker`] handles created (Java `registered`)
    registered: i32,
}

/// Per-worker element counts, written only by the owning worker.
#[derive(Default)]
#[repr(align(128))]
struct WorkerCounts([AtomicI64; 5]);

/// Coordinates workers moving through nodes -> ways -> relations. See the [module docs](self).
pub struct OsmPhaser {
    state: Mutex<PhaserState>,
    advanced: Condvar,
    start_times: [OnceLock<Timer>; 5],
    counts: Mutex<Vec<Arc<WorkerCounts>>>,
}

impl OsmPhaser {
    /// Creates a new phaser expecting a certain number of workers to register.
    pub fn new(workers: usize) -> OsmPhaser {
        let workers = workers as i32;
        OsmPhaser {
            state: Mutex::new(PhaserState { phase: 0, parties: workers, unarrived: workers, registered: 0 }),
            advanced: Condvar::new(),
            start_times: Default::default(),
            counts: Mutex::new(Vec::new()),
        }
    }

    /// Number of nodes processed so far.
    pub fn nodes(&self) -> i64 {
        self.get_count(Phase::Nodes)
    }

    /// Number of ways processed so far.
    pub fn ways(&self) -> i64 {
        self.get_count(Phase::Ways)
    }

    /// Number of relations processed so far.
    pub fn relations(&self) -> i64 {
        self.get_count(Phase::Relations)
    }

    fn get_count(&self, phase: Phase) -> i64 {
        self.counts.lock().iter().map(|c| c.0[phase as usize].load(Ordering::Relaxed)).sum()
    }

    fn worker_started(&self, phase: Phase) {
        self.start_times[phase as usize].get_or_init(Timer::start);
    }

    fn all_workers_finished(&self, phase: Phase) {
        if let Some(timer) = self.start_times[phase as usize].get() {
            timer.stop();
        }
        if let Some(summary) = self.get_summary(phase) {
            tracing::info!("Finished {summary}");
        }
    }

    fn get_summary(&self, phase: Phase) -> Option<String> {
        let timer = self.start_times[phase as usize].get()?;
        let count = self.get_count(phase);
        if count > 0 {
            let rate = count as f64 / timer.elapsed().wall.as_secs_f64();
            Some(format!("{phase}: {} ({}/s) in {timer}", format::integer(count), format::numeric(rate, false)))
        } else {
            None
        }
    }

    /// Prints how many elements were processed per phase, how long they took, and how many elements per second were
    /// processed.
    pub fn print_summary(&self) {
        for phase in [Phase::Nodes, Phase::Ways, Phase::Relations] {
            tracing::debug!("  {}", self.get_summary(phase).unwrap_or_else(|| "null".to_string()));
        }
    }

    /// Indicate that `workers` workers will be registering eventually, so that if one starts early it doesn't advance
    /// through phases before it knows about the other workers.
    pub fn register_workers(&self, workers: usize) {
        let mut state = self.state.lock();
        state.parties += workers as i32;
        state.unarrived += workers as i32;
    }

    /// Returns a new [`ForWorker`] handle for a worker to use to coordinate with other workers.
    ///
    /// If [`OsmPhaser::register_workers`] has not been called yet, or called with a smaller number, this will
    /// automatically register another worker.
    pub fn for_worker(&self) -> ForWorker<'_> {
        {
            let mut state = self.state.lock();
            state.registered += 1;
            if state.registered > state.parties {
                state.parties += 1;
                state.unarrived += 1;
            }
        }
        let counts = Arc::new(WorkerCounts::default());
        self.counts.lock().push(counts.clone());
        ForWorker {
            phaser: self,
            finish_actions: Vec::new(),
            current_phase: Phase::Begin,
            counts,
            count: 0,
            skipped_phase: true,
        }
    }

    /// The current overall phase (all workers have finished the phases before it).
    pub fn get_phase(&self) -> Phase {
        Phase::from(self.state.lock().phase)
    }

    fn phase_number(&self) -> i32 {
        self.state.lock().phase
    }

    /// Java `Phaser.arrive()`: arrive at the current phase without waiting.
    fn arrive(&self) {
        let mut state = self.state.lock();
        state.unarrived -= 1;
        if state.unarrived <= 0 {
            self.all_workers_finished(Phase::from(state.phase));
            state.phase += 1;
            state.unarrived = state.parties;
            self.advanced.notify_all();
        }
    }

    /// Java `Phaser.awaitAdvance(phase)`: wait until the phaser moves past phase number `phase`.
    fn await_advance(&self, phase: i32) {
        let mut state = self.state.lock();
        while state.phase == phase {
            self.advanced.wait(&mut state);
        }
    }
}

type FinishAction<'a> = Box<dyn FnOnce() -> anyhow::Result<()> + Send + 'a>;

/// Handle for a worker to use to coordinate with other workers processing OSM elements.
///
/// The worker keeps track of the current phase in thread-local state, and coordinates with other threads only when
/// the phase changes.
pub struct ForWorker<'a> {
    phaser: &'a OsmPhaser,
    finish_actions: Vec<(Phase, FinishAction<'a>)>,
    current_phase: Phase,
    counts: Arc<WorkerCounts>,
    /// elements processed in the current phase (mirrored into `counts`)
    count: i64,
    // we don't increment a counter until after an element is processed, so need to keep track of if we skipped the
    // current phase so that we don't increment the counter when the phase is finished
    skipped_phase: bool,
}

impl<'a> ForWorker<'a> {
    /// Register `action` to run after this worker finished `phase`.
    pub fn when_worker_finishes(
        mut self,
        phase: Phase,
        action: impl FnOnce() -> anyhow::Result<()> + Send + 'a,
    ) -> Self {
        self.finish_actions.push((phase, Box::new(action)));
        self
    }

    /// The phase this worker is currently in.
    pub fn current_phase(&self) -> Phase {
        self.current_phase
    }

    #[inline]
    fn inc(&mut self) {
        self.count += 1;
        // only this worker writes to its counts, so avoid an atomic read-modify-write
        self.counts.0[self.current_phase as usize].store(self.count, Ordering::Relaxed);
    }

    #[inline]
    fn advance(&mut self, new_phase: Phase, wait_for_others: bool, is_skip: bool) -> anyhow::Result<()> {
        if self.current_phase == new_phase {
            // normal case - still working on same phase
            self.inc();
            Ok(())
        } else {
            self.advance_slow(new_phase, wait_for_others, is_skip)
        }
    }

    #[cold]
    fn advance_slow(&mut self, new_phase: Phase, wait_for_others: bool, is_skip: bool) -> anyhow::Result<()> {
        if new_phase < self.current_phase {
            anyhow::bail!(
                "Elements must be sorted with nodes first, then ways, then relations. Encountered {new_phase} after {}",
                self.current_phase
            );
        }
        // advance to next phase, but first, check if we skipped over some phases
        while self.current_phase.next() != new_phase {
            self.advance(self.current_phase.next(), true, true)?;
        }
        let current = self.current_phase;
        let mut i = 0;
        while i < self.finish_actions.len() {
            if self.finish_actions[i].0 == current {
                let (_, action) = self.finish_actions.remove(i);
                action()?;
            } else {
                i += 1;
            }
        }
        // increment the counter for the last element processed from the last phase, unless the last phase was BEGIN,
        // or we skipped over the last phase.
        if current != Phase::Begin && !self.skipped_phase {
            self.inc();
        }
        self.skipped_phase = is_skip;

        self.current_phase = current.next();
        self.count = 0;
        let phaser = self.phaser;

        // don't let a worker move to next phase if other workers aren't even on that phase yet
        if phaser.phase_number() < self.current_phase.number() - 1 {
            phaser.await_advance(self.current_phase.prev().prev().number());
        }
        phaser.arrive();
        if wait_for_others {
            phaser.await_advance(self.current_phase.prev().number());
        }
        phaser.worker_started(self.current_phase);
        // don't increment the counter since we want the count to go up after each element was processed (not before).
        Ok(())
    }

    /// Start processing an element in `phase` without waiting for other workers to start on the phase.
    ///
    /// If this is the first element in that phase, then the previous phase is marked as done which triggers any
    /// handler registered through [`ForWorker::when_worker_finishes`]. If this was the last worker on the previous
    /// phase, then the overall phase will advance into `phase` now.
    ///
    /// This could block if we need to wait for other workers to get to the previous phase before we leave it.
    ///
    /// Fails if `phase` is before the current phase (elements are out of order).
    #[inline]
    pub fn arrive(&mut self, phase: Phase) -> anyhow::Result<()> {
        self.advance(phase, false, false)
    }

    /// Wait for all workers to get to this point before starting to process the first element in `phase`.
    #[inline]
    pub fn arrive_and_wait_for_others(&mut self, phase: Phase) -> anyhow::Result<()> {
        self.advance(phase, true, false)
    }

    /// Same as calling [`ForWorker::arrive_and_wait_for_others`] `n` times.
    #[inline]
    pub fn arrive_and_wait_for_others_many(&mut self, phase: Phase, n: usize) -> anyhow::Result<()> {
        if n > 0 {
            self.advance(phase, true, false)?;
            self.count += n as i64 - 1;
            self.counts.0[self.current_phase as usize].store(self.count, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Mark this worker as done (Java `close()`).
    pub fn close(&mut self) -> anyhow::Result<()> {
        self.advance(Phase::Done, false, false)
    }
}

impl Drop for ForWorker<'_> {
    /// Workers that exit early (i.e. on error) still arrive at every remaining phase so other workers waiting on
    /// them don't block forever.
    fn drop(&mut self) {
        if self.current_phase != Phase::Done {
            let _ = self.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::*;

    #[test]
    fn test_advance_single_thread() {
        let phaser = OsmPhaser::new(0);
        let mut for_worker = phaser.for_worker();
        assert_eq!(Phase::Begin, phaser.get_phase());

        // multiple calls stays put
        for_worker.arrive(Phase::Nodes).unwrap();
        assert_eq!(Phase::Nodes, phaser.get_phase());
        assert_eq!(0, phaser.nodes());
        for_worker.arrive(Phase::Nodes).unwrap();
        assert_eq!(Phase::Nodes, phaser.get_phase());
        assert_eq!(1, phaser.nodes());

        for_worker.arrive_and_wait_for_others(Phase::Ways).unwrap();
        assert_eq!(2, phaser.nodes());
        assert_eq!(Phase::Ways, phaser.get_phase());
        for_worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
        assert_eq!(Phase::Relations, phaser.get_phase());
        for_worker.close().unwrap();
        assert_eq!(Phase::Done, phaser.get_phase());

        assert_eq!(2, phaser.nodes());
        assert_eq!(1, phaser.ways());
        assert_eq!(1, phaser.relations());
    }

    #[test]
    fn test_advance_and_skip_phases() {
        let phaser = OsmPhaser::new(1);
        let mut for_worker = phaser.for_worker();
        assert_eq!(Phase::Begin, phaser.get_phase());

        for_worker.arrive(Phase::Nodes).unwrap();
        assert_eq!(0, phaser.nodes(), "don't increment count before processing an element");
        assert_eq!(Phase::Nodes, phaser.get_phase());

        for_worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
        assert_eq!(1, phaser.nodes(), "increment count until after processing an element");
        assert_eq!(Phase::Relations, phaser.get_phase());

        for_worker.close().unwrap();

        assert_eq!(1, phaser.nodes());
        assert_eq!(0, phaser.ways());
        assert_eq!(1, phaser.relations());
        assert_eq!(Phase::Done, phaser.get_phase());
    }

    #[test]
    fn test_worker_advance_side_effect() {
        let nodes_called = AtomicBool::new(false);
        let ways_called = AtomicBool::new(false);
        let phaser = OsmPhaser::new(0);

        let mut for_worker = phaser
            .for_worker()
            .when_worker_finishes(Phase::Nodes, || {
                nodes_called.store(true, Ordering::SeqCst);
                Ok(())
            })
            .when_worker_finishes(Phase::Ways, || {
                ways_called.store(true, Ordering::SeqCst);
                Ok(())
            });
        assert!(!nodes_called.load(Ordering::SeqCst));
        assert!(!ways_called.load(Ordering::SeqCst));

        for_worker.arrive(Phase::Ways).unwrap();
        assert!(nodes_called.load(Ordering::SeqCst));
        assert!(!ways_called.load(Ordering::SeqCst));

        for_worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
        assert!(ways_called.load(Ordering::SeqCst));

        for_worker.close().unwrap();
    }

    #[test]
    fn test_cant_go_backwards() {
        let phaser = OsmPhaser::new(1);
        let mut for_worker = phaser.for_worker();
        for_worker.arrive(Phase::Ways).unwrap();
        assert!(for_worker.arrive(Phase::Nodes).is_err());
        for_worker.close().unwrap();
        assert!(for_worker.arrive(Phase::Relations).is_err());
    }

    #[test]
    fn test_wait_to_advance() {
        let phaser = OsmPhaser::new(2);
        let (latch1_tx, latch1) = crossbeam_channel::unbounded::<()>();
        let (latch2_tx, latch2) = crossbeam_channel::unbounded::<()>();
        let (latch3_tx, latch3) = crossbeam_channel::unbounded::<()>();
        let working_on_relations = AtomicBool::new(false);
        let working_on_ways = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let t1 = scope.spawn(|| {
                let mut worker = phaser.for_worker();
                worker.arrive(Phase::Nodes).unwrap();
                worker.arrive(Phase::Ways).unwrap();
                latch2_tx.send(()).unwrap();
                working_on_ways.store(true, Ordering::SeqCst);
                worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
                working_on_relations.store(true, Ordering::SeqCst);
                worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
                worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
                worker.close().unwrap();
            });
            let t2 = scope.spawn(|| {
                let mut worker = phaser.for_worker();
                worker.arrive(Phase::Nodes).unwrap();
                latch1.recv().unwrap();
                worker.arrive(Phase::Ways).unwrap();
                latch3.recv().unwrap();
                worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
                worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
                worker.arrive_and_wait_for_others(Phase::Relations).unwrap();
                worker.close().unwrap();
            });

            latch2.recv_timeout(Duration::from_secs(1)).unwrap(); // proves thread 1 advanced to ways before thread 2
            latch1_tx.send(()).unwrap(); // let thread 2 continue on to ways

            // wait and make sure thread 1 is still waiting on arrive_and_wait_for_others(RELATIONS)
            std::thread::sleep(Duration::from_millis(100));
            assert!(working_on_ways.load(Ordering::SeqCst));
            assert!(!working_on_relations.load(Ordering::SeqCst));

            latch3_tx.send(()).unwrap(); // now let thread 2 move onto relations, unblock thread 1 and both finish
            t1.join().unwrap();
            t2.join().unwrap();
        });
        assert_eq!(Phase::Done, phaser.get_phase());
        assert_eq!(2, phaser.nodes());
        assert_eq!(2, phaser.ways());
        assert_eq!(6, phaser.relations());
    }
}
