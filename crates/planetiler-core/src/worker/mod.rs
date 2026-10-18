//! Port of the `worker` package: utilities for running work in parallel threads and handing it off between them.

pub mod distributor;
pub mod weighted_handoff_queue;
pub mod work_queue;
// Named after the Java class `Worker` that it ports, like every other module in this package.
#[allow(clippy::module_inception)]
pub mod worker;
pub mod worker_pipeline;

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

/// An error shared between everything waiting on a [`Done`] handle.
#[derive(Clone)]
pub struct SharedError(pub Arc<anyhow::Error>);

impl fmt::Debug for SharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl fmt::Display for SharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() { write!(f, "{:#}", self.0) } else { fmt::Display::fmt(&*self.0, f) }
    }
}

impl std::error::Error for SharedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// Result that a [`Done`] handle completes with.
pub type DoneResult = Result<(), SharedError>;

type Listener = Box<dyn FnOnce(&DoneResult) + Send>;

#[derive(Default)]
struct DoneState {
    result: Option<DoneResult>,
    listeners: Vec<Listener>,
}

/// A handle to wait for a group of threads to complete successfully or fail; a minimal stand-in for Java's
/// `CompletableFuture<Void>`. Clones share the same state.
#[derive(Clone, Default)]
pub struct Done {
    inner: Arc<(Mutex<DoneState>, Condvar)>,
}

impl fmt::Debug for Done {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Done").field("result", &self.inner.0.lock().result).finish()
    }
}

impl Done {
    /// Returns a new incomplete handle.
    pub fn new() -> Done {
        Done::default()
    }

    /// Returns a handle that is already completed successfully.
    pub fn completed() -> Done {
        let done = Done::new();
        done.complete(Ok(()));
        done
    }

    /// Completes this handle with `result` if it is not already complete. Returns true if this call completed it.
    pub fn complete(&self, result: DoneResult) -> bool {
        let listeners = {
            let mut state = self.inner.0.lock();
            if state.result.is_some() {
                return false;
            }
            state.result = Some(result.clone());
            self.inner.1.notify_all();
            std::mem::take(&mut state.listeners)
        };
        for listener in listeners {
            listener(&result);
        }
        true
    }

    /// Completes this handle with an error.
    pub fn fail(&self, error: anyhow::Error) -> bool {
        self.complete(Err(SharedError(Arc::new(error))))
    }

    /// Returns true if this handle has completed (successfully or not).
    pub fn is_done(&self) -> bool {
        self.inner.0.lock().result.is_some()
    }

    /// Returns the result if complete.
    pub fn result(&self) -> Option<DoneResult> {
        self.inner.0.lock().result.clone()
    }

    /// Runs `listener` when this completes (or immediately if it already has).
    pub fn on_complete(&self, listener: impl FnOnce(&DoneResult) + Send + 'static) {
        let mut state = self.inner.0.lock();
        if let Some(result) = state.result.clone() {
            drop(state);
            listener(&result);
        } else {
            state.listeners.push(Box::new(listener));
        }
    }

    /// Returns a new handle that completes after this one completes successfully and `action` runs, or fails with the
    /// same error as this one (Java's `thenRun`).
    pub fn then_run(&self, action: impl FnOnce() -> anyhow::Result<()> + Send + 'static) -> Done {
        let next = Done::new();
        let n = next.clone();
        self.on_complete(move |result| match result {
            Ok(()) => {
                n.complete(action().map_err(|e| SharedError(Arc::new(e))));
            }
            Err(e) => {
                n.complete(Err(e.clone()));
            }
        });
        next
    }

    /// Returns a handle that completes successfully when all `handles` complete, or fails as soon as the first one
    /// fails (Java's `Worker.joinFutures`).
    pub fn join_all(handles: impl IntoIterator<Item = Done>) -> Done {
        let handles: Vec<Done> = handles.into_iter().collect();
        let result = Done::new();
        if handles.is_empty() {
            result.complete(Ok(()));
            return result;
        }
        let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(handles.len()));
        for handle in handles {
            let result = result.clone();
            let remaining = remaining.clone();
            handle.on_complete(move |r| match r {
                Err(e) => {
                    result.complete(Err(e.clone()));
                }
                Ok(()) => {
                    if remaining.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                        result.complete(Ok(()));
                    }
                }
            });
        }
        result
    }

    /// Blocks until complete, returning an error if it failed.
    pub fn wait(&self) -> anyhow::Result<()> {
        let mut state = self.inner.0.lock();
        while state.result.is_none() {
            self.inner.1.wait(&mut state);
        }
        to_anyhow(state.result.clone().unwrap())
    }

    /// Blocks until complete or `timeout` elapses. Returns `None` on timeout.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<anyhow::Result<()>> {
        let deadline = Instant::now() + timeout;
        let mut state = self.inner.0.lock();
        while state.result.is_none() {
            if self.inner.1.wait_until(&mut state, deadline).timed_out() {
                break;
            }
        }
        state.result.clone().map(to_anyhow)
    }
}

fn to_anyhow(result: DoneResult) -> anyhow::Result<()> {
    result.map_err(anyhow::Error::new)
}

/// Converts a panic payload to an error.
pub(crate) fn panic_to_error(payload: Box<dyn std::any::Any + Send>) -> anyhow::Error {
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    };
    anyhow::anyhow!("panic: {msg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_join() {
        let a = Done::new();
        let b = Done::new();
        let all = Done::join_all([a.clone(), b.clone()]);
        assert!(all.wait_timeout(Duration::from_millis(1)).is_none());
        a.complete(Ok(()));
        assert!(!all.is_done());
        b.complete(Ok(()));
        assert!(all.wait().is_ok());
    }

    #[test]
    fn done_join_fails_fast() {
        let a = Done::new();
        let b = Done::new();
        let all = Done::join_all([a.clone(), b.clone()]);
        a.fail(anyhow::anyhow!("boom"));
        let err = all.wait().unwrap_err();
        assert_eq!("boom", err.to_string());
        assert!(!b.is_done());
    }

    #[test]
    fn then_run() {
        let a = Done::new();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r = ran.clone();
        let next = a.then_run(move || {
            r.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        a.complete(Ok(()));
        next.wait().unwrap();
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));

        let failed = Done::new();
        let next = failed.then_run(|| panic!("should not run"));
        failed.fail(anyhow::anyhow!("err"));
        assert!(next.wait().is_err());
    }
}
