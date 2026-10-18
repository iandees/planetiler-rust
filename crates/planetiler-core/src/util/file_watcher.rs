//! Port of `util/FileWatcher.java`: watches a set of paths so that each call to [`FileWatcher::poll`] returns the set
//! of paths that have been modified since the last call.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Polls a set of files for changes to their modification time.
#[derive(Debug, Default)]
pub struct FileWatcher {
    modification_times: BTreeMap<PathBuf, Option<SystemTime>>,
}

/// Returns the absolute, lexically-normalized form of `path` (without resolving symlinks, like Java's
/// `toAbsolutePath().normalize()`).
pub fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf())
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other),
        }
    }
    result
}

impl FileWatcher {
    /// Returns a new file watcher watching a set of files for modifications.
    pub fn new_watcher<P: AsRef<Path>>(paths: &[P]) -> FileWatcher {
        let mut watcher = FileWatcher::default();
        for path in paths {
            watcher.watch(path.as_ref());
        }
        watcher
    }

    /// Returns the (normalized) paths modified since the last call to poll.
    pub fn poll(&mut self) -> BTreeSet<PathBuf> {
        let paths: Vec<PathBuf> = self.modification_times.keys().cloned().collect();
        paths.into_iter().filter(|p| self.watch(p)).collect()
    }

    /// Adds `path` to the set of paths to check on each call to [`FileWatcher::poll`]. Returns true if the
    /// modification time changed since the last time it was checked.
    pub fn watch(&mut self, path: &Path) -> bool {
        let path = normalize(path);
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let last = self.modification_times.insert(path, modified);
        last != Some(modified)
    }

    /// Removes `path` from the set of paths to check.
    pub fn unwatch(&mut self, path: &Path) {
        self.modification_times.remove(&normalize(path));
    }

    /// Returns true if we are currently watching `path` for changes.
    pub fn watching(&self, path: &Path) -> bool {
        self.modification_times.contains_key(&normalize(path))
    }

    /// Ensures we are only watching `paths`. Does nothing if `paths` is `None` or empty.
    pub fn set_watched(&mut self, paths: Option<&BTreeSet<PathBuf>>) {
        let Some(paths) = paths.filter(|p| !p.is_empty()) else {
            return;
        };
        let paths: BTreeSet<PathBuf> = paths.iter().map(|p| normalize(p)).collect();
        for to_watch in &paths {
            if !self.watching(to_watch) {
                self.watch(to_watch);
            }
        }
        self.modification_times.retain(|p, _| paths.contains(p));
    }

    /// Blocks and invokes `action` every time one of the watched files changes, checking every `delay`. `action`
    /// returns the new set of files to watch. Returns when `action` returns an error.
    pub fn poll_for_changes(
        &mut self,
        delay: Duration,
        mut action: impl FnMut(BTreeSet<PathBuf>) -> anyhow::Result<BTreeSet<PathBuf>>,
    ) -> anyhow::Result<()> {
        loop {
            let changes = self.poll();
            if !changes.is_empty() {
                let watched = action(changes)?;
                self.set_watched(Some(&watched));
            }
            std::thread::sleep(delay);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        a: PathBuf,
        b: PathBuf,
        c: PathBuf,
        time: u64,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let p = |n: &str| normalize(&dir.path().join(n));
            Fixture { a: p("a"), b: p("b"), c: p("c"), _dir: dir, time: 1 }
        }

        fn touch(&mut self, paths: &[&PathBuf]) {
            for path in paths {
                std::fs::write(path, []).unwrap();
                let file = std::fs::File::options().write(true).open(path).unwrap();
                let time = SystemTime::UNIX_EPOCH + Duration::from_millis(self.time);
                file.set_times(FileTimes::new().set_modified(time)).unwrap();
                self.time += 1;
            }
        }
    }

    fn set(paths: &[&PathBuf]) -> BTreeSet<PathBuf> {
        paths.iter().map(|p| (*p).clone()).collect()
    }

    #[test]
    fn watch() {
        let mut f = Fixture::new();
        let (a, b) = (f.a.clone(), f.b.clone());
        f.touch(&[&a]);
        let mut watcher = FileWatcher::new_watcher(&[&a, &b]);
        assert_eq!(set(&[]), watcher.poll());
        assert_eq!(set(&[]), watcher.poll());
        f.touch(&[&a]);
        assert_eq!(set(&[&a]), watcher.poll());
        f.touch(&[&b]);
        assert_eq!(set(&[&b]), watcher.poll());
        f.touch(&[&a, &b]);
        assert_eq!(set(&[&a, &b]), watcher.poll());
    }

    #[test]
    fn remove_watch() {
        let mut f = Fixture::new();
        let (a, b) = (f.a.clone(), f.b.clone());
        f.touch(&[&a, &b]);
        let mut watcher = FileWatcher::new_watcher(&[&a, &b]);
        assert_eq!(set(&[]), watcher.poll());
        watcher.unwatch(&a);
        f.touch(&[&a, &b]);
        assert_eq!(set(&[&b]), watcher.poll());
        watcher.unwatch(&b);
        f.touch(&[&a, &b]);
        assert_eq!(set(&[]), watcher.poll());
    }

    #[test]
    fn return_watched() {
        let mut f = Fixture::new();
        let (a, b, c) = (f.a.clone(), f.b.clone(), f.c.clone());
        f.touch(&[&a]);
        let mut watcher = FileWatcher::new_watcher(&[&a, &b]);
        assert_eq!(set(&[]), watcher.poll());
        assert_eq!(set(&[]), watcher.poll());
        f.touch(&[&a]);
        assert_eq!(set(&[&a]), watcher.poll());

        watcher.set_watched(Some(&set(&[&b, &c])));
        f.touch(&[&b]);
        assert_eq!(set(&[&b]), watcher.poll());
        f.touch(&[&a]);
        assert_eq!(set(&[]), watcher.poll());
        f.touch(&[&a, &b, &c]);
        assert_eq!(set(&[&b, &c]), watcher.poll());

        watcher.set_watched(Some(&set(&[&a])));
        f.touch(&[&b]);
        assert_eq!(set(&[]), watcher.poll());
        f.touch(&[&a]);
        assert_eq!(set(&[&a]), watcher.poll());
        f.touch(&[&a, &b, &c]);
        assert_eq!(set(&[&a]), watcher.poll());

        watcher.set_watched(Some(&set(&[])));
        f.touch(&[&a, &b, &c]);
        assert_eq!(set(&[&a]), watcher.poll());
        watcher.set_watched(None);
        f.touch(&[&a, &b, &c]);
        assert_eq!(set(&[&a]), watcher.poll());
    }
}
